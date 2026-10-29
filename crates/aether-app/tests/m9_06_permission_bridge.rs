//! M9-06 权限/对话框桥（daemon 业务层 E2E；P2 计划 M9-06 DoD-1/2/3/5）。
//!
//! 覆盖：
//! - **DoD-1 权限回环 E2E**：真实 Mock 适配器进程 `permission.request` →
//!   `aether-app`（daemon 的业务层，`IpcState::dispatch`）→ `permissions_pending`
//!   （UI 轮询）→ `permission_resolve`（唯一决议入口）→ 适配器 `permission.resolved`；
//!   allow / deny / 300s 超时 deny / 取消；审计落库；零直通探针。
//! - **DoD-2**：`session_pause`/`session_resume`/`session_interrupt`/`run_retry`
//!   在同一通道下语义不变；`pause_unsupported` 能力门。
//! - **DoD-3**：选择器返回的路径由后续 `artifact_add` 复核（canonicalize + 存在性）。
//! - **DoD-5**：`permissions_pending` 轮询与 `permission.pending` 通知并存，
//!   通知不产生第二条决议路径（重复决议 → `permission_not_pending`）。
//!
//! 边界：`aetherd` 二进制的运行时注册清单要求构建期摘要（ADR-015），集成测试
//! 无法注入 Mock 适配器；本文件使用与 `boot::build_backend` **同一装配**（监督器 →
//! 会话管理器 → `AdapterRunExecutor` → `SessionBackend` → `IpcState::dispatch`），
//! JSON-RPC 路由/进程级通知由 `crates/aether-daemon/tests/m9_06_permission_pending.rs`
//! 与 M9-04 路由矩阵覆盖。
//!
//! 环境：`AETHER_MOCK_ADAPTER=<Bun 编译产物>`（`pnpm verify:m9-06` 构建并设置）；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_app::adapter_executor::{AdapterRunExecutor, ExecutorPermissionStats};
use aether_app::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_app::event_bridge::SinkError;
use aether_app::ipc::backend::NotImplementedBackend;
use aether_app::ipc::IpcState;
use aether_app::permission_loop::PermissionServiceGate;
use aether_app::permission_notice::{
    DispatchPendingCount, PermissionNotice, PermissionNoticeBridge, PermissionNoticeHandle,
    PermissionNoticeSink,
};
use aether_app::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_app::session_backend::SessionBackend;
use aether_app::shutdown::StorageSlot;
use aether_control::{
    EventPipeline, LifecycleConfig, ManualClock, PermissionConfig, PermissionService,
    SessionManager, SharedClock, SystemClock,
};
use aether_core::{RunId, RunStatus, SessionId, SessionStatus};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, WriteQueue};
use serde_json::{json, Value};
use tempfile::TempDir;

/// 进程/适配器用例串行闸门（每个用例 spawn 真实适配器进程，并行会拖慢握手断言）。
fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    match SERIAL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m9-06 构建 Mock 后执行）"
            );
            None
        }
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

/// canonical 路径 → 目标字符串形态（剥离 `\\?\` 扩展前缀；该前缀属 D9 拒绝形态）。
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// 区外写目标（缺省 `workspace_write` 档：区内写 allow、区外写 ask）。
fn outside_target(dir: &Path, name: &str) -> String {
    plain(
        &std::fs::canonicalize(dir)
            .expect("canonicalize dir")
            .join(name),
    )
}

#[derive(Default)]
struct RecordingSink {
    notices: Mutex<Vec<PermissionNotice>>,
}

impl RecordingSink {
    fn snapshot(&self) -> Vec<PermissionNotice> {
        match self.notices.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl PermissionNoticeSink for RecordingSink {
    fn send(&self, notice: &PermissionNotice) -> Result<(), SinkError> {
        match self.notices.lock() {
            Ok(mut guard) => guard.push(notice.clone()),
            Err(poisoned) => poisoned.into_inner().push(notice.clone()),
        }
        Ok(())
    }
}

struct Harness {
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    pipeline: EventPipeline,
    reads: ReadPool,
    write: WriteQueue,
    manager: SessionManager,
    executor: Arc<AdapterRunExecutor>,
    supervisor: Arc<Supervisor>,
    service: PermissionService,
    clock: Arc<ManualClock>,
    state: IpcState,
    sink: Arc<RecordingSink>,
    notice: Option<PermissionNoticeHandle>,
    storage: Arc<StorageSlot>,
}

fn harness(binary: &Path, with_pause_unsupported_runtime: bool) -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    let mut specs = vec![RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf())
            .official(true)
            .with_args(["--stream-deltas", "4", "--stream-interval-ms", "1"]),
    )];
    if with_pause_unsupported_runtime {
        // M7-03/ADR-017：未声明 `pause_resume` 的能力门负向夹具（官方白名单 id）。
        specs.push(RuntimeSpec::with_fresh_token(
            RuntimeManifest::new("pi", "Pi", binary.to_path_buf())
                .official(true)
                .with_args(["--no-pause-resume"]),
        ));
    }
    let supervisor = Arc::new(
        boot_supervisor(specs, Some(&dir.path().join("adapters.json"))).expect("构造监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "{runtime_id} 预热必须 Ready：{outcome:?}"
        );
    }
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let policy = PolicyEngine::new(dir.path()).expect("策略引擎");
    let shared_clock: SharedClock = clock.clone();
    let service = PermissionService::new(
        PermissionConfig {
            // 测试经 `sweep_timeouts_once`/`ManualClock` 驱动 300s 边界，不真实等待。
            wait_timeout: None,
            ..PermissionConfig::default()
        },
        shared_clock,
        policy,
        write.clone(),
        reads.clone(),
        pipeline.clone(),
    );
    let gate = PermissionServiceGate::new(service.clone());
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        Some(gate),
    ));
    let mut manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    let notifier: Arc<dyn aether_control::PauseNotifier> = executor.clone();
    manager = manager.with_pause_notifier(notifier);
    let _ = manager.spawn_background(&handle);
    let backend: Arc<dyn aether_app::ipc::IpcBackend> = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager.clone()),
            Some(executor.clone()),
            Some(reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle.clone(),
        )
        .with_permissions(service.clone())
        .with_workspace_store(write.clone()),
    );
    let state = IpcState::new(backend, vec![dir.path().to_path_buf()]);
    let sink = Arc::new(RecordingSink::default());
    // DoD-5：通知桥与轮询并存（同一 `permissions_pending` 事实源）。
    let notice = PermissionNoticeBridge::new(
        pipeline.clone(),
        Arc::new(DispatchPendingCount::new(state.clone())),
        sink.clone(),
    )
    .spawn(&handle);
    Harness {
        dir,
        runtime,
        pipeline,
        reads,
        write,
        manager,
        executor,
        supervisor,
        service,
        clock,
        state,
        sink,
        notice: Some(notice),
        storage,
    }
}

impl Harness {
    fn call(&self, method: &str, params: Value) -> Value {
        self.state.dispatch(method, params).unwrap_or_else(|error| {
            panic!(
                "{method} 应成功，实际 {}：{}",
                error.code.as_str(),
                error.message
            )
        })
    }

    fn call_err(&self, method: &str, params: Value) -> aether_app::ipc::IpcError {
        self.state
            .dispatch(method, params)
            .expect_err(&format!("{method} 必须失败"))
    }

    fn create_session(&self, runtime_id: &str, title: &str) -> String {
        let created = self.call(
            "session_create",
            json!({ "runtime_id": runtime_id, "title": title }),
        );
        created["id"].as_str().expect("会话 id").to_owned()
    }

    fn send(&self, session_id: &str, text: &str, client_msg_id: &str) -> String {
        let ack = self.call(
            "session_send",
            json!({
                "session_id": session_id,
                "text": text,
                "client_msg_id": client_msg_id,
            }),
        );
        ack["run_id"].as_str().expect("run id").to_owned()
    }

    fn pending(&self) -> Vec<Value> {
        self.call("permissions_pending", json!({}))
            .as_array()
            .cloned()
            .expect("pending 数组")
    }

    async fn wait_pending(&self, timeout: Duration) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let pending = self.pending();
            if !pending.is_empty() {
                return pending;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待权限待审批超时（permission.request 未进入队列）"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_pending_cleared(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.pending().is_empty() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待待审批清空超时（取消/超时未收口）"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_session_status(
        &self,
        session_id: &SessionId,
        expected: SessionStatus,
        timeout: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self.session_status(session_id).await;
            if status == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待会话状态 {expected:?} 超时，实际 {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_notice(
        &self,
        predicate: impl Fn(&PermissionNotice) -> bool,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.sink.snapshot().iter().any(&predicate) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_run(&self, run_id: &RunId, expected: RunStatus, timeout: Duration) -> RunStatus {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self
                .reads
                .run(run_id)
                .await
                .expect("run 读取")
                .map(|run| run.status);
            if status == Some(expected) {
                return expected;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("等待 run {run_id} 到 {expected:?} 超时，实际 {status:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn session_status(&self, session_id: &SessionId) -> SessionStatus {
        self.manager
            .session_status(session_id)
            .await
            .expect("会话状态")
    }

    async fn audit_actions(&self) -> Vec<String> {
        self.reads
            .audit_log(500)
            .await
            .expect("审计读取")
            .into_iter()
            .map(|record| record.action)
            .collect()
    }

    async fn event_types(&self, session_id: &SessionId) -> Vec<String> {
        self.pipeline
            .readback(session_id, 0)
            .await
            .expect("补读")
            .events
            .iter()
            .map(|event| event.event_type().as_str().to_owned())
            .collect()
    }

    /// 等待回环下发完成（适配器终态事件与 `permission.resolve` 下发之间有异步窗口）。
    async fn wait_permission_stats(&self, timeout: Duration) -> ExecutorPermissionStats {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let stats = self.executor.permission_loop_stats();
            if stats.zero_passthrough || tokio::time::Instant::now() >= deadline {
                return stats;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 等待某类事件落盘（适配器终态事件与 run 行状态之间允许异步投递窗口）。
    async fn wait_event(
        &self,
        session_id: &SessionId,
        expected: &str,
        timeout: Duration,
    ) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let types = self.event_types(session_id).await;
            if types.iter().any(|item| item == expected) {
                return types;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待事件 {expected} 超时，实际 {types:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn shutdown(self) {
        let Harness {
            dir,
            runtime,
            mut notice,
            manager,
            supervisor,
            pipeline,
            reads,
            write,
            storage,
            ..
        } = self;
        drop(dir);
        runtime.block_on(async {
            if let Some(handle) = notice.as_mut() {
                let _ = handle.shutdown(Duration::from_secs(2)).await;
            }
            manager.shutdown_background().await;
            supervisor.shutdown_all().await;
            let _ = pipeline.shutdown().await;
        });
        drop((reads, write));
        if let Some(storage) = storage.take() {
            let _ = runtime.block_on(storage.shutdown());
        }
        drop(runtime);
    }
}

fn target_outside(name: &str) -> (TempDir, String) {
    let outside = tempfile::tempdir().expect("区外目录");
    let target = outside_target(outside.path(), name);
    std::fs::write(&target, b"x").expect("写目标");
    (outside, target)
}

/// DoD-1/DoD-5：allow 回环 + 通知/轮询并存 + 审计 + 零直通 + 重复决议拒绝。
#[test]
fn permission_loop_allow_flows_from_adapter_to_ui_and_back() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, false);
    let runtime = &h.runtime;
    runtime.block_on(async {
        let session_id = h.create_session("mock", "M9-06 allow");
        let session = SessionId::new(session_id.clone()).expect("session id");
        let (_outside, target) = target_outside("allow.md");
        let run_id = RunId::new(h.send(
            &session_id,
            &format!("permission-loop:{target}"),
            "01J8ZQ5R0N7W9Y8X6V4T2S0K60",
        ))
        .expect("run id");

        let pending = h.wait_pending(Duration::from_secs(30)).await;
        assert_eq!(pending.len(), 1, "同一会话同时最多 1 个待审批");
        assert_eq!(pending[0]["resource"], json!("fs.write"));
        assert_eq!(pending[0]["target"], json!(target), "原始 target 原样上报");
        let request_id = pending[0]["request_id"]
            .as_str()
            .expect("request id")
            .to_owned();

        // DoD-5：通知与轮询并存；通知只带 count/session_id。
        assert!(
            h.wait_notice(
                |notice| notice.count == 1
                    && notice.session_id.as_deref() == Some(session_id.as_str()),
                Duration::from_secs(15)
            )
            .await,
            "permission.requested 必须触发 permission.pending(count=1)"
        );
        assert_eq!(h.pending().len(), 1, "轮询重复读取不产生重复票据");

        let resolved = h.call(
            "permission_resolve",
            json!({ "request_id": request_id, "decision": "once" }),
        );
        assert_eq!(resolved["decision"], json!("allow"));
        assert_eq!(resolved["scope"], json!("once"));

        h.wait_run(&run_id, RunStatus::Succeeded, Duration::from_secs(30))
            .await;
        assert!(h.pending().is_empty(), "决议后待审批清空");
        assert!(
            h.wait_notice(|notice| notice.count == 0, Duration::from_secs(15))
                .await,
            "permission.resolved 必须触发 permission.pending(count=0)"
        );

        let stats = h.wait_permission_stats(Duration::from_secs(15)).await;
        assert!(stats.zero_passthrough, "{}", stats.summary());
        assert_eq!(stats.snapshot.requests_received, 1);
        assert_eq!(stats.snapshot.decisions, 1);
        assert_eq!(stats.snapshot.resolutions_sent, 1);
        assert_eq!(stats.snapshot.gate_failures, 0);

        let types = h.event_types(&session).await;
        assert!(
            types.contains(&"permission.requested".to_owned()),
            "{types:?}"
        );
        assert!(
            types.contains(&"permission.resolved".to_owned()),
            "{types:?}"
        );
        let actions = h.audit_actions().await;
        for expected in ["permission.requested", "permission.resolved"] {
            assert!(actions.contains(&expected.to_owned()), "{actions:?}");
        }

        // DoD-5：唯一决议路径；重复决议 → permission_not_pending（通知不参与决议）。
        let error = h.call_err(
            "permission_resolve",
            json!({ "request_id": request_id, "decision": "deny" }),
        );
        assert_eq!(error.code.as_str(), "invalid_value");
        assert!(
            error.message.contains("permission_not_pending"),
            "{}",
            error.message
        );
        assert_eq!(h.pending().len(), 0);
    });
    h.shutdown();
}

/// DoD-1：deny 回环（适配器收到 deny → tool.call_failed；审计落库）。
#[test]
fn permission_loop_deny_is_delivered_to_the_adapter() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, false);
    let runtime = &h.runtime;
    runtime.block_on(async {
        let session_id = h.create_session("mock", "M9-06 deny");
        let session = SessionId::new(session_id.clone()).expect("session id");
        let (_outside, target) = target_outside("deny.md");
        let run_id = RunId::new(h.send(
            &session_id,
            &format!("permission-loop:{target}"),
            "01J8ZQ5R0N7W9Y8X6V4T2S0K61",
        ))
        .expect("run id");

        let pending = h.wait_pending(Duration::from_secs(30)).await;
        let request_id = pending[0]["request_id"]
            .as_str()
            .expect("request id")
            .to_owned();
        let resolved = h.call(
            "permission_resolve",
            json!({ "request_id": request_id, "decision": "deny" }),
        );
        assert_eq!(resolved["decision"], json!("deny"));
        assert_eq!(resolved["scope"], Value::Null);

        h.wait_run(&run_id, RunStatus::Succeeded, Duration::from_secs(30))
            .await;
        let types = h
            .wait_event(&session, "tool.call_failed", Duration::from_secs(15))
            .await;
        assert!(
            types.contains(&"permission.resolved".to_owned()),
            "{types:?}"
        );
        let actions = h.audit_actions().await;
        assert!(
            actions.contains(&"permission.resolved".to_owned()),
            "{actions:?}"
        );
        let stats = h.wait_permission_stats(Duration::from_secs(15)).await;
        assert!(stats.zero_passthrough, "{}", stats.summary());
        assert_eq!(stats.snapshot.resolutions_sent, 1);
    });
    h.shutdown();
}

/// DoD-1：300s 超时 → deny + 审计 `permission.timeout`；299.999s 不超时。
#[test]
fn permission_loop_timeout_denies_at_three_hundred_seconds() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, false);
    let runtime = &h.runtime;
    runtime.block_on(async {
        let session_id = h.create_session("mock", "M9-06 timeout");
        let (_outside, target) = target_outside("timeout.md");
        let run_id = RunId::new(h.send(
            &session_id,
            &format!("permission-loop:{target}"),
            "01J8ZQ5R0N7W9Y8X6V4T2S0K62",
        ))
        .expect("run id");
        h.wait_pending(Duration::from_secs(30)).await;

        h.clock.advance(299_999);
        assert!(
            h.service.sweep_timeouts_once().await.is_empty(),
            "299.999s 不得判超时"
        );
        h.clock.advance(1);
        let timed_out = h.service.sweep_timeouts_once().await;
        assert_eq!(timed_out.len(), 1, "恰好 300s 必须判超时 deny");

        h.wait_run(&run_id, RunStatus::Succeeded, Duration::from_secs(30))
            .await;
        assert!(h.pending().is_empty());
        let actions = h.audit_actions().await;
        assert!(
            actions.contains(&"permission.timeout".to_owned()),
            "{actions:?}"
        );
        let stats = h.wait_permission_stats(Duration::from_secs(15)).await;
        assert!(stats.zero_passthrough, "{}", stats.summary());
        assert_eq!(stats.snapshot.resolutions_sent, 1);
        assert!(
            h.wait_notice(|notice| notice.count == 0, Duration::from_secs(15))
                .await,
            "超时 deny 同样触发通知刷新"
        );
    });
    h.shutdown();
}

/// DoD-1/DoD-2：取消（interrupt）→ 权限等待按 deny 收口 + `permission.cancelled` 审计；
/// 取消后的 run 可 `run_retry`（终态语义不变）。
#[test]
fn permission_loop_cancel_and_run_retry_keep_channel_semantics() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, false);
    let runtime = &h.runtime;
    runtime.block_on(async {
        let session_id = h.create_session("mock", "M9-06 cancel");
        let (_outside, target) = target_outside("cancel.md");
        let run_id = RunId::new(h.send(
            &session_id,
            &format!("permission-loop:{target}"),
            "01J8ZQ5R0N7W9Y8X6V4T2S0K63",
        ))
        .expect("run id");
        h.wait_pending(Duration::from_secs(30)).await;

        let interrupted = h.call("session_interrupt", json!({ "session_id": session_id }));
        assert_eq!(interrupted["interrupted_run"], json!(run_id.as_str()));
        h.wait_run(&run_id, RunStatus::Cancelled, Duration::from_secs(30))
            .await;
        h.wait_pending_cleared(Duration::from_secs(15)).await;
        let actions = h.audit_actions().await;
        assert!(
            actions.contains(&"permission.cancelled".to_owned()),
            "{actions:?}"
        );
        let stats = h.wait_permission_stats(Duration::from_secs(15)).await;
        assert!(stats.zero_passthrough, "{}", stats.summary());
        assert_eq!(stats.snapshot.resolutions_sent, 1, "取消必须回传 deny 决议");
        // run_retry：仅终态（cancelled）可重试；复用原输入消息、新 run。
        let retried = h.call("run_retry", json!({ "run_id": run_id.as_str() }));
        assert_eq!(retried["session_id"], json!(session_id));
        assert_eq!(retried["queued"], json!(false));
        assert_ne!(retried["run_id"], json!(run_id.as_str()));
        let retry_run =
            RunId::new(retried["run_id"].as_str().expect("retry run")).expect("retry run id");
        // 重放再次经过权限回环（同一通道语义）：等待新 pending 后中断收口。
        let pending = h.wait_pending(Duration::from_secs(30)).await;
        let retry_request = pending[0]["request_id"].as_str().expect("retry request");
        let retry_request = retry_request.to_owned();
        let interrupted = h.call("session_interrupt", json!({ "session_id": session_id }));
        assert_eq!(interrupted["interrupted_run"], json!(retry_run.as_str()));
        h.wait_run(&retry_run, RunStatus::Cancelled, Duration::from_secs(30))
            .await;
        // 重复决议（已由取消收口）→ permission_not_pending。
        let error = h.call_err(
            "permission_resolve",
            json!({ "request_id": retry_request, "decision": "once" }),
        );
        assert_eq!(error.code.as_str(), "invalid_value");
        assert!(
            error.message.contains("permission_not_pending"),
            "{}",
            error.message
        );
        h.wait_pending_cleared(Duration::from_secs(15)).await;
    });
    h.shutdown();
}

/// DoD-2：`session_pause`/`session_resume` 语义（受控中断 + 重放）与
/// `pause_unsupported` 能力门（零状态转移）。
#[test]
fn pause_resume_and_capability_gate_are_unchanged() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, true);
    let runtime = &h.runtime;
    runtime.block_on(async {
        // ① 能力门：`pi` 未声明 `pause_resume` → `pause_unsupported`，零状态转移。
        let idle_session = h.create_session("pi", "M9-06 pause-unsupported");
        let idle_id = SessionId::new(idle_session.clone()).expect("session id");
        let error = h.call_err("session_pause", json!({ "session_id": idle_session }));
        assert_eq!(error.code.as_str(), "pause_unsupported");
        assert_eq!(h.session_status(&idle_id).await, SessionStatus::Idle);

        // ② `mock` 声明能力：在途 run 受控中断 → paused；恢复重放 → running。
        let session_id = h.create_session("mock", "M9-06 pause");
        let session = SessionId::new(session_id.clone()).expect("session id");
        let run_id =
            RunId::new(h.send(&session_id, "long", "01J8ZQ5R0N7W9Y8X6V4T2S0K64")).expect("run id");
        h.wait_session_status(&session, SessionStatus::Running, Duration::from_secs(30))
            .await;
        let paused = h.call("session_pause", json!({ "session_id": session_id }));
        assert_eq!(paused["status"], json!("paused"));
        assert_eq!(paused["from"], json!("running"));
        assert_eq!(paused["interrupted_run"], json!(run_id.as_str()));
        h.wait_run(&run_id, RunStatus::Cancelled, Duration::from_secs(30))
            .await;
        h.wait_session_status(&session, SessionStatus::Paused, Duration::from_secs(15))
            .await;

        let resumed = h.call("session_resume", json!({ "session_id": session_id }));
        assert_eq!(resumed["status"], json!("running"));
        assert_eq!(resumed["from"], json!("paused"));
        let replay =
            RunId::new(resumed["run_id"].as_str().expect("replay run")).expect("replay run id");
        h.wait_session_status(&session, SessionStatus::Running, Duration::from_secs(30))
            .await;

        // 收口：中断重放 run，恢复空闲后释放。
        let interrupted = h.call("session_interrupt", json!({ "session_id": session_id }));
        assert_eq!(interrupted["interrupted_run"], json!(replay.as_str()));
        h.wait_run(&replay, RunStatus::Cancelled, Duration::from_secs(30))
            .await;
    });
    h.shutdown();
}

/// DoD-3：选择器只返回路径；后续 `artifact_add` 复核 canonicalize/存在性。
#[test]
fn picked_paths_are_rechecked_by_artifact_add() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let _serial = serial_guard();
    let h = harness(&binary, false);
    let runtime = &h.runtime;
    runtime.block_on(async {
        let session_id = h.create_session("mock", "M9-06 artifacts");
        let outside = tempfile::tempdir().expect("引用目录");
        let picked = outside.path().join("picked.md");
        std::fs::write(&picked, b"picked").expect("写文件");
        let canonical = plain(&std::fs::canonicalize(&picked).expect("canonical"));
        let added = h.call(
            "artifact_add",
            json!({ "session_id": session_id, "path": canonical }),
        );
        assert_eq!(added["path"], json!(canonical));
        assert_eq!(added["kind"], json!("file"));

        // 选择后文件被移走：artifact_add 复核拒绝（选择器不构成信任）。
        let base = std::fs::canonicalize(outside.path()).expect("dir");
        let missing = base.join("missing.md");
        let missing_canonical = plain(&missing);
        let error = h.call_err(
            "artifact_add",
            json!({ "session_id": session_id, "path": missing_canonical }),
        );
        assert_eq!(error.code.as_str(), "artifact_path_rejected");
        assert!(!missing.exists());
    });
    h.shutdown();
}
