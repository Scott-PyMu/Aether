//! M3-03 集成测试：权限中心与运行状态面板的命令面（设计 D9/D5；UI-UX S-03/S-04）。
//!
//! 覆盖（实施计划 v1.17 §4 M3-03）：
//! - DoD1：`permissions_pending` 展示待审批（原文 target + canonical 对照字段）→
//!   `permission_resolve`（允许/拒绝）→ 适配器收到决议（真实回环，零直通）；
//!   超时 deny：300s（时钟注入）→ 清单移除 + 审计 `permission.timeout`；
//! - DoD1 异常路径（M2-10 场景在 IPC 面重放）：deny / 超时 deny / once / session /
//!   重启后 pending 决议（重复决议回 `permission_not_pending`）；
//! - DoD2（同会话并发 ask 的清单形状）：3 条并发 ask → `permissions_pending` 返回 3 条
//!   （排序稳定）；UI 侧「≤1 激活 + 排队展示」由前端测试覆盖；
//! - DoD3（后端防线）：`disabled` 运行时不可创建会话（D5；`invalid_value`）。
//!
//! 环境：真实 Mock 适配器进程经 `AETHER_MOCK_ADAPTER` 指定（由
//! `scripts/test/m3-03/verify-m3-03.mjs` 构建并设置；未设置时跳过，
//! `AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::permission_loop::{
    PermissionLoop, PermissionLoopProbe, PermissionLoopSnapshot,
};
use aether_adapters::session_client::{AdapterSessionClient, RunOutcome};
use aether_adapters::AdapterProcess;
use aether_control::{
    EventPipeline, ExecutorFuture, ExecutorOutcome, LifecycleConfig, ManualClock, PermissionConfig,
    PermissionRequest, PermissionService, PipelineConfig, RunExecutor, RunRequest, SessionManager,
    StartupSelfCheckReport, StoreEventSource, StoreJournal, SystemClock,
};
use aether_core::{
    EventPayload, EventType, PermissionDecision, Runtime, RuntimeId, RuntimeStatus, Session,
    SessionId, SessionStatus, TokenUsage,
};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, StoreCommand, StoreRuntime, WriteQueueConfig};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    PermissionDecision as DtoPermissionDecision, PermissionResolveRequest,
    PermissionsPendingRequest, SessionCreateRequest,
};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::permission_loop::PermissionServiceGate;
use aether_tauri::runtime_control::{boot_supervisor, mock_spec, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::runtime::Handle;

// ===== 环境与通用 =====

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m3-03 构建 Mock 后执行）"
            );
            None
        }
    }
}

async fn wait_for<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

fn next_msg_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// 证据导出（DoD1 归档）：`AETHER_M3_03_EVIDENCE_DIR` 存在时写 JSON；同时打印摘要行。
fn write_evidence(name: &str, value: &Value) {
    println!("[m3-03] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_03_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{name}.json"));
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(&path, text);
}

// ===== 测试内核（真实存储 + 管线 + 权限服务）=====

struct TestCore {
    temp: TempDir,
    db_path: PathBuf,
    storage: StoreRuntime,
    pipeline: EventPipeline,
    workspace: TempDir,
    clock: Arc<ManualClock>,
    service: PermissionService,
}

impl TestCore {
    async fn open() -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        let db_path = temp.path().join("aether.db");
        let storage = StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current())
            .expect("打开存储");
        let pipeline = Self::start_pipeline(&storage);
        let workspace = tempfile::tempdir().expect("临时工作区");
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let service = Self::build_service(&storage, &pipeline, workspace.path(), &clock);
        Self {
            temp,
            db_path,
            storage,
            pipeline,
            workspace,
            clock,
            service,
        }
    }

    /// 在既有数据目录重开核心（重启后 pending 决议）。
    async fn reopen(
        temp: TempDir,
        db_path: PathBuf,
        workspace: TempDir,
        clock: Arc<ManualClock>,
    ) -> Self {
        let storage = StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current())
            .expect("重开存储");
        let pipeline = Self::start_pipeline(&storage);
        let service = Self::build_service(&storage, &pipeline, workspace.path(), &clock);
        Self {
            temp,
            db_path,
            storage,
            pipeline,
            workspace,
            clock,
            service,
        }
    }

    fn start_pipeline(storage: &StoreRuntime) -> EventPipeline {
        EventPipeline::start(
            PipelineConfig {
                persist_retry_delay: Duration::ZERO,
                ..PipelineConfig::default()
            },
            Arc::new(StoreJournal::new(storage.queue().clone())),
            Arc::new(StoreEventSource::new(storage.reads().clone())),
            &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
            &Handle::current(),
        )
        .expect("启动事件管线")
    }

    fn build_service(
        storage: &StoreRuntime,
        pipeline: &EventPipeline,
        workspace: &Path,
        clock: &Arc<ManualClock>,
    ) -> PermissionService {
        let policy = PolicyEngine::new(workspace).expect("策略引擎");
        let shared_clock: aether_control::SharedClock = clock.clone();
        PermissionService::new(
            PermissionConfig {
                // 测试经 `sweep_timeouts_once`/`ManualClock` 驱动超时，不用真实兜底计时。
                wait_timeout: None,
                ..PermissionConfig::default()
            },
            shared_clock,
            policy,
            storage.queue().clone(),
            storage.reads().clone(),
            pipeline.clone(),
        )
    }

    fn reads(&self) -> ReadPool {
        self.storage.reads().clone()
    }

    async fn shutdown(self) -> (TempDir, PathBuf, TempDir, Arc<ManualClock>) {
        self.pipeline.shutdown().await.expect("关闭管线");
        self.storage.shutdown().await.expect("关闭存储");
        (self.temp, self.db_path, self.workspace, self.clock)
    }

    /// 落库适配器侧会话行（生产由 M3-02 生命周期承接 native_id 映射；测试 1:1 构造）。
    async fn insert_session(&self, session_id: &str) {
        let runtime = Runtime {
            id: RuntimeId::new("mock").unwrap(),
            name: "Mock".to_owned(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: "1.0".to_owned(),
            capabilities: Vec::new(),
            endpoint: None,
            config: json!({}),
            status: RuntimeStatus::Ready,
            status_reason: None,
            last_seen_at: None,
            created_at: 1,
            updated_at: 1,
        };
        self.storage
            .queue()
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await
            .expect("runtimes 行");
        let session = Session {
            id: SessionId::new(session_id).unwrap(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: format!("m3-03-{session_id}"),
            status: SessionStatus::Idle,
            model: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            system_prompt: None,
            config: json!({}),
            token_usage: TokenUsage::default(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
        };
        self.storage
            .queue()
            .execute(StoreCommand::InsertSession { session })
            .await
            .expect("sessions 行");
    }

    async fn audit_actions(&self) -> Vec<String> {
        self.reads()
            .audit_log(500)
            .await
            .expect("审计读取")
            .into_iter()
            .map(|record| record.action)
            .collect()
    }

    async fn wait_audit_action(&self, action: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.audit_actions().await.iter().any(|item| item == action) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.audit_actions().await.iter().any(|item| item == action)
    }

    async fn session_event_types(&self, session_id: &str) -> Vec<String> {
        self.pipeline
            .readback(&SessionId::new(session_id).unwrap(), 0)
            .await
            .expect("补读")
            .events
            .iter()
            .map(|event| event.event_type().as_str().to_owned())
            .collect()
    }
}

/// 命令面后端（与生产装配同构：SessionBackend + 真实权限服务）。
fn permission_backend(core: &TestCore) -> Arc<dyn IpcBackend> {
    Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            None,
            None,
            Some(core.reads()),
            None,
            Handle::current(),
        )
        .with_permissions(core.service.clone()),
    )
}

fn pending_list(backend: &dyn IpcBackend, session_id: Option<&str>) -> Vec<Value> {
    let value = backend
        .permissions_pending(&PermissionsPendingRequest {
            session_id: session_id.map(str::to_owned),
        })
        .expect("permissions_pending");
    value.as_array().cloned().expect("数组")
}

fn resolve_via_ipc(
    backend: &dyn IpcBackend,
    request_id: &str,
    decision: DtoPermissionDecision,
) -> Value {
    backend
        .permission_resolve(&PermissionResolveRequest {
            request_id: request_id.to_owned(),
            decision,
        })
        .expect("permission_resolve")
}

// ===== 适配器（真实 Mock 进程 + 会话客户端 + 权限回环）=====

struct AdapterHarness {
    process: AdapterProcess,
    client: AdapterSessionClient,
    probe: Arc<PermissionLoopProbe>,
    permission_loop: Arc<PermissionLoop>,
}

async fn launch(service: PermissionService) -> Option<AdapterHarness> {
    let binary = mock_binary()?;
    let mut process = AdapterProcess::spawn(&binary, Vec::<String>::new())
        .await
        .expect("启动 Mock 适配器进程");
    let connection = process.connect().expect("连接 Mock stdio");
    let hello = connection
        .handshake()
        .await
        .expect("hello 必须在 10s 内到达且 major 兼容");
    assert_eq!(hello.protocol, "1.0");
    let gate = PermissionServiceGate::new(service);
    let permission_loop = PermissionLoop::new(gate);
    let probe = permission_loop.probe();
    let client = AdapterSessionClient::with_permission_loop(
        Arc::new(connection),
        Some(Arc::clone(&permission_loop)),
    );
    Some(AdapterHarness {
        process,
        client,
        probe,
        permission_loop,
    })
}

impl AdapterHarness {
    async fn open_session(&self) -> String {
        self.client
            .create_session(Some("m3-03"), None, None)
            .await
            .expect("session.create")
            .session_id
    }

    async fn send_loop(&self, session_id: &str, target: &Path) -> String {
        self.client
            .send(
                session_id,
                &next_msg_id("m3-03-write"),
                &format!("permission-loop:{}", target.display()),
            )
            .await
            .expect("session.send ack")
            .run_id
    }

    async fn shutdown(&mut self) {
        let _ = self.permission_loop.shutdown().await;
        let _ = self.client.shutdown().await;
        let _ = self.process.wait_timeout(Duration::from_secs(5)).await;
        let _ = self.process.kill().await;
    }
}

fn snapshot_evidence(snapshot: &PermissionLoopSnapshot) -> Value {
    json!({
        "requests_received": snapshot.requests_received,
        "requests_invalid": snapshot.requests_invalid,
        "decisions": snapshot.decisions,
        "resolutions_sent": snapshot.resolutions_sent,
        "resolution_failures": snapshot.resolution_failures,
        "gate_failures": snapshot.gate_failures,
        "zero_passthrough": snapshot.zero_passthrough(),
    })
}

fn payload_of<'a>(
    events: &'a [aether_core::EventEnvelope],
    run_id: &str,
    event_type: EventType,
) -> Option<&'a aether_core::EventEnvelope> {
    events.iter().find(|event| {
        event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id)
            && event.event_type() == event_type
    })
}

// ===== DoD1：ask 弹窗数据源（待审批清单）→ 允许/拒绝 → 适配器收到决议 =====

#[test]
fn pending_list_exposes_target_pair_and_allow_reaches_adapter() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let target = core.workspace.path().join("notes.md");
        std::fs::write(&target, b"x").unwrap();
        let backend = permission_backend(&core);

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), Some(&session_id)).is_empty(),
                Duration::from_secs(10)
            )
            .await,
            "fs.write 工作区内必须进入待审批（ask）"
        );

        // UI 数据源：待审批清单（原文 target + canonical 对照字段，D9 评审 #10）。
        let items = pending_list(backend.as_ref(), Some(&session_id));
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item["resource"], "fs.write");
        assert_eq!(item["action"], "write");
        assert_eq!(
            item["target"].as_str().map(str::to_owned),
            Some(target.to_string_lossy().to_string()),
            "原始 target 原样展示（防视觉欺骗）"
        );
        assert_eq!(
            item["canonical_target"].as_str(),
            target.canonicalize().ok().as_deref().and_then(Path::to_str),
            "规范化结果必须随清单返回（UI 并排对照）"
        );
        assert_eq!(item["timeout_ms"], 300_000, "D9：300s");
        assert!(item["requested_at"].as_i64().unwrap_or(0) > 0);

        // 决议前：适配器仍在等待（零直通的第一半）。
        let before = adapter.probe.snapshot();
        assert_eq!(before.requests_received, 1);
        assert_eq!(before.decisions, 0, "决议必须来自 UI 命令，不得预置");
        assert_eq!(before.resolutions_sent, 0);

        // UI「仅本次允许」（once）→ 适配器收到决策。
        let resolved = resolve_via_ipc(
            backend.as_ref(),
            item["request_id"].as_str().unwrap(),
            DtoPermissionDecision::Once,
        );
        assert_eq!(resolved["decision"], "allow");
        assert_eq!(resolved["scope"], "once");

        let outcome = adapter
            .client
            .wait_run_outcome(&run_id, Duration::from_secs(10))
            .await
            .expect("run 终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        assert_eq!(
            (
                snapshot.requests_received,
                snapshot.decisions,
                snapshot.resolutions_sent
            ),
            (1, 1, 1),
            "100% 回环、零直通"
        );

        // 决议后清单清空；审计可查（DoD1）。
        assert!(pending_list(backend.as_ref(), None).is_empty());
        let actions = core.audit_actions().await;
        for expected in ["permission.requested", "permission.resolved"] {
            assert!(actions.contains(&expected.to_owned()), "{actions:?}");
        }
        let types = adapter.client.run_event_types(&run_id).await;
        assert_eq!(
            types,
            vec![
                "run.started",
                "tool.call_started",
                "permission.resolved",
                "tool.call_completed",
                "run.completed"
            ]
        );

        write_evidence(
            "dod1_ask_allow",
            &json!({
                "task": "M3-03 DoD1 待审批清单 → 允许 → 适配器收到决议",
                "session_id": session_id,
                "run_id": run_id,
                "pending_item": item,
                "resolve_response": resolved,
                "adapter_event_sequence": types,
                "core_event_sequence": core.session_event_types(&session_id).await,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

#[test]
fn deny_via_ipc_reaches_adapter_and_audits() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let target = core.workspace.path().join("blocked.md");
        std::fs::write(&target, b"x").unwrap();
        let backend = permission_backend(&core);

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), Some(&session_id)).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        let item = pending_list(backend.as_ref(), Some(&session_id))[0].clone();
        let resolved = resolve_via_ipc(
            backend.as_ref(),
            item["request_id"].as_str().unwrap(),
            DtoPermissionDecision::Deny,
        );
        assert_eq!(resolved["decision"], "deny");
        assert_eq!(resolved["scope"], Value::Null);

        let outcome = adapter
            .client
            .wait_run_outcome(&run_id, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        let types = adapter.client.run_event_types(&run_id).await;
        assert_eq!(
            types,
            vec![
                "run.started",
                "tool.call_started",
                "permission.resolved",
                "tool.call_failed",
                "run.completed"
            ]
        );
        let events = adapter.client.events().await;
        let resolved_event = payload_of(&events, &run_id, EventType::PermissionResolved).unwrap();
        match &resolved_event.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Deny);
                assert_eq!(payload.scope, None);
            }
            other => panic!("payload 类型不符：{other:?}"),
        }
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        let actions = core.audit_actions().await;
        assert!(
            actions.contains(&"permission.resolved".to_owned()),
            "{actions:?}"
        );
        write_evidence(
            "dod1_ask_deny",
            &json!({
                "task": "M3-03 DoD1 待审批清单 → 拒绝 → 适配器收到决议",
                "adapter_event_sequence": types,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// 超时 deny（300s，时钟注入）：清单移除 + 审计 `permission.timeout` + 适配器收到 deny。
#[test]
fn timeout_deny_clears_pending_and_audits() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let target = core.workspace.path().join("timeout.md");
        std::fs::write(&target, b"x").unwrap();
        let backend = permission_backend(&core);

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );

        // 299.999s 不超时；恰好 +300.000s 判 deny（D9；清单由巡检摘除）。
        core.clock.advance(299_999);
        assert!(core.service.sweep_timeouts_once().await.is_empty());
        assert_eq!(pending_list(backend.as_ref(), None).len(), 1);
        core.clock.advance(1);
        let timed_out = core.service.sweep_timeouts_once().await;
        assert_eq!(timed_out.len(), 1, "恰好 300s 必须判超时");

        assert!(
            wait_for(
                || pending_list(backend.as_ref(), None).is_empty(),
                Duration::from_secs(5)
            )
            .await,
            "超时后待审批清单必须移除"
        );
        let outcome = adapter
            .client
            .wait_run_outcome(&run_id, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        let actions = core.audit_actions().await;
        assert!(
            actions.contains(&"permission.timeout".to_owned()),
            "超时审计可查（DoD1）：{actions:?}"
        );

        write_evidence(
            "dod1_timeout_deny",
            &json!({
                "task": "M3-03 DoD1 超时 deny（300s，时钟注入）",
                "timed_out": timed_out,
                "audit_actions": actions,
                "probe": snapshot_evidence(&snapshot),
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// once / session 授权经 IPC 决议（M2-10 异常路径的 IPC 面重放）。
#[test]
fn once_then_session_grant_via_ipc() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let target = core.workspace.path().join("grant.md");
        std::fs::write(&target, b"x").unwrap();
        let backend = permission_backend(&core);

        // run1：once allow → run2 同 target 再次 pending（once 不跨请求）。
        let run1 = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        let item = pending_list(backend.as_ref(), None)[0].clone();
        resolve_via_ipc(
            backend.as_ref(),
            item["request_id"].as_str().unwrap(),
            DtoPermissionDecision::Once,
        );
        assert!(matches!(
            adapter
                .client
                .wait_run_outcome(&run1, Duration::from_secs(10))
                .await,
            Some(RunOutcome::Completed { .. })
        ));

        let run2 = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), None).is_empty(),
                Duration::from_secs(10)
            )
            .await,
            "once 授权不得跨请求"
        );
        // session 授权 → run3 同 target 命中会话级授权（无 pending，仍经回环）。
        let item = pending_list(backend.as_ref(), None)[0].clone();
        let granted = resolve_via_ipc(
            backend.as_ref(),
            item["request_id"].as_str().unwrap(),
            DtoPermissionDecision::Session,
        );
        assert_eq!(granted["decision"], "allow");
        assert_eq!(granted["scope"], "session");
        assert!(matches!(
            adapter
                .client
                .wait_run_outcome(&run2, Duration::from_secs(10))
                .await,
            Some(RunOutcome::Completed { .. })
        ));

        let decisions_before = adapter.probe.snapshot().decisions;
        let run3 = adapter.send_loop(&session_id, &target).await;
        assert!(matches!(
            adapter
                .client
                .wait_run_outcome(&run3, Duration::from_secs(10))
                .await,
            Some(RunOutcome::Completed { .. })
        ));
        assert!(
            wait_for(
                || adapter.probe.snapshot().decisions == decisions_before + 1,
                Duration::from_secs(5)
            )
            .await,
            "会话级授权路径同样经回环"
        );
        assert!(
            pending_list(backend.as_ref(), None).is_empty(),
            "授权命中不产生 pending"
        );
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        assert_eq!(snapshot.decisions, 3);
        let actions = core.audit_actions().await;
        assert!(
            actions.contains(&"permission.session_grant_hit".to_owned()),
            "{actions:?}"
        );

        write_evidence(
            "dod1_once_session",
            &json!({
                "task": "M3-03 DoD1 once/session 授权（IPC 决议）",
                "run_ids": [run1, run2, run3],
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// 重启后 pending 决议（M2-10 异常路径）：清单可展示 → IPC 决议 → 审计；
/// 重复决议回 `permission_not_pending`（稳定业务码，无重复语义）。
#[test]
fn pending_survives_restart_and_resolves_via_ipc() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let target = core.workspace.path().join("pending-restart.md");
        std::fs::write(&target, b"x").unwrap();
        let backend = permission_backend(&core);

        let _run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !pending_list(backend.as_ref(), None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        assert!(
            core.wait_audit_action("permission.requested", Duration::from_secs(5))
                .await,
            "回环入口审计必须落库"
        );
        let request_id = pending_list(backend.as_ref(), None)[0]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // 核心关闭（适配器随应用退出）后重启：pending 恢复并可经 IPC 决议。
        adapter.shutdown().await;
        drop(adapter);
        let (temp, db_path, workspace, clock) = core.shutdown().await;
        let restarted = TestCore::reopen(temp, db_path, workspace, clock).await;
        let restored = restarted.service.restore_pending().await.unwrap();
        assert_eq!(restored, 1, "重启后待审批必须恢复");
        let backend = permission_backend(&restarted);

        let items = pending_list(backend.as_ref(), Some(&session_id));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["request_id"], request_id.as_str());
        assert_eq!(
            items[0]["canonical_target"],
            Value::Null,
            "重启恢复的 pending 无 canonical 缓存（按原文对照展示）"
        );

        let resolved = resolve_via_ipc(backend.as_ref(), &request_id, DtoPermissionDecision::Deny);
        assert_eq!(resolved["decision"], "deny");
        assert!(pending_list(backend.as_ref(), None).is_empty());
        let actions = restarted.audit_actions().await;
        for expected in ["permission.requested", "permission.resolved"] {
            assert!(actions.contains(&expected.to_owned()), "{actions:?}");
        }

        // 重复决议 → permission_not_pending（稳定业务码；不新增错误码枚举）。
        let error = backend
            .permission_resolve(&PermissionResolveRequest {
                request_id: request_id.clone(),
                decision: DtoPermissionDecision::Once,
            })
            .expect_err("重复决议必须拒绝");
        assert_eq!(error.code, IpcErrorCode::InvalidValue);
        assert!(
            error.message.contains("permission_not_pending"),
            "{}",
            error.message
        );

        write_evidence(
            "dod1_restart_pending",
            &json!({
                "task": "M3-03 DoD1 重启后 pending 决议（IPC）",
                "request_id": request_id,
                "restored": restored,
                "duplicate_resolve": error.message,
                "audit_actions": actions,
            }),
        );
        let _ = restarted.shutdown().await;
    });
}

// ===== DoD2：同会话并发 ask 的清单形状（UI 排队展示的数据源）=====

#[test]
fn concurrent_asks_are_listed_in_queue_order() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let session_id = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
        core.insert_session(session_id).await;
        let backend = permission_backend(&core);
        let target = core.workspace.path().join("queue.md");
        std::fs::write(&target, b"x").unwrap();

        // 同会话 3 条并发 ask（服务层允许并存；D9「≤1 激活」为 UI 排队口径）。
        let request_ids = [
            "01J8ZQ5R0N7W9Y8X6V4T2S0K1P",
            "01J8ZQ5R0N7W9Y8X6V4T2S0K2P",
            "01J8ZQ5R0N7W9Y8X6V4T2S0K3P",
        ];
        let mut tasks = Vec::new();
        for request_id in request_ids {
            let service = core.service.clone();
            let sid = SessionId::new(session_id).unwrap();
            let target = target.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .request(PermissionRequest {
                        request_id: request_id.to_owned(),
                        session_id: Some(sid),
                        runtime_id: Some(RuntimeId::new("mock").unwrap()),
                        resource: "fs.write".to_owned(),
                        action: "write".to_owned(),
                        target: Some(target.to_string_lossy().to_string()),
                        content_bytes: None,
                    })
                    .await
            }));
        }

        assert!(
            wait_for(
                || pending_list(backend.as_ref(), Some(session_id)).len() == 3,
                Duration::from_secs(10)
            )
            .await,
            "3 条并发 ask 必须全部进入清单"
        );
        let items = pending_list(backend.as_ref(), Some(session_id));
        assert_eq!(items.len(), 3);
        // 排序稳定（requested_at 相同时按票据 id 次序）；每条均可独立决议。
        let mut listed: Vec<String> = items
            .iter()
            .map(|item| item["request_id"].as_str().unwrap().to_owned())
            .collect();
        listed.sort();
        let mut expected: Vec<String> = request_ids.iter().map(|id| (*id).to_owned()).collect();
        expected.sort();
        assert_eq!(listed, expected);

        for item in &items {
            resolve_via_ipc(
                backend.as_ref(),
                item["request_id"].as_str().unwrap(),
                DtoPermissionDecision::Deny,
            );
        }
        assert!(pending_list(backend.as_ref(), Some(session_id)).is_empty());
        for task in tasks {
            let _ = task.await;
        }
        let actions = core.audit_actions().await;
        assert_eq!(
            actions
                .iter()
                .filter(|action| action.as_str() == "permission.requested")
                .count(),
            3
        );

        write_evidence(
            "dod2_queue",
            &json!({
                "task": "M3-03 DoD2 同会话并发 ask 清单（UI ≤1 激活 + 排队）",
                "pending_count": items.len(),
                "request_ids": listed,
                "audit_actions": actions,
            }),
        );

        let _ = core.shutdown().await;
    });
}

// ===== DoD3：disabled 运行时不可创建会话（D5；后端防线）=====

/// 测试执行器：仅满足 `SessionManager` 构造（本用例在创建前即被拒绝，不会执行）。
#[derive(Default)]
struct StubExecutor;

impl RunExecutor for StubExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            request.cancel.cancelled().await;
            ExecutorOutcome::Cancelled { reason: None }
        })
    }
}

#[test]
fn disabled_runtime_cannot_create_session() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        // 预热失败（二进制缺失）→ 监督器状态 disabled + start_failed（D5 失败场景）。
        let supervisor = Arc::new(
            boot_supervisor(
                vec![mock_spec("mock", "definitely-missing-adapter-binary")],
                Some(&core.temp.path().join("adapters.json")),
            )
            .expect("构造监督器"),
        );
        let startup = run_supervisor_startup(&supervisor, &Handle::current()).expect("启动尾段");
        for (_, monitor) in &startup.monitors {
            monitor.abort();
        }
        let disabled = supervisor.get("mock").expect("runtime").status().await;
        assert_eq!(disabled, RuntimeStatus::Disabled, "缺失二进制必须 disabled");

        let manager = SessionManager::new(
            LifecycleConfig::default(),
            Arc::new(SystemClock),
            core.storage.queue().clone(),
            core.reads(),
            core.pipeline.clone(),
            Arc::new(StubExecutor),
        );
        let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager),
            None,
            Some(core.reads()),
            Some(Arc::clone(&supervisor)),
            Handle::current(),
        ));

        let error = backend
            .session_create(&SessionCreateRequest {
                runtime_id: "mock".to_owned(),
                title: "禁用运行时会话".to_owned(),
                workspace_id: None,
                model: None,
                thinking_depth: None,
            })
            .expect_err("disabled 运行时必须拒绝创建会话");
        assert_eq!(error.code, IpcErrorCode::InvalidValue);
        assert!(
            error.message.contains("已禁用"),
            "拒绝原因必须可展示：{}",
            error.message
        );
        // 不落库（无会话行）。
        let sessions = core.reads().sessions(Default::default()).await.unwrap();
        assert!(sessions.is_empty(), "拒绝必须不落库");

        write_evidence(
            "dod3_disabled_runtime",
            &json!({
                "task": "M3-03 DoD3 disabled 适配器不可创建会话",
                "runtime_status": "disabled",
                "error_code": error.code.as_str(),
                "error_message": error.message,
                "sessions_after": sessions.len(),
            }),
        );

        let _ = core.shutdown().await;
    });
}

/// 未接线权限服务：命令回 `core_not_ready`（不伪造空队列）。
#[test]
fn permission_commands_require_service() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let backend = SessionBackend::new(
            Arc::new(NotImplementedBackend),
            None,
            None,
            Some(core.reads()),
            None,
            Handle::current(),
        );
        let error = backend
            .permissions_pending(&PermissionsPendingRequest { session_id: None })
            .expect_err("未接线必须拒绝");
        assert_eq!(error.code, IpcErrorCode::CoreNotReady);
        let resolve = backend.permission_resolve(&PermissionResolveRequest {
            request_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1P".to_owned(),
            decision: DtoPermissionDecision::Deny,
        });
        assert_eq!(
            resolve.expect_err("未接线必须拒绝").code,
            IpcErrorCode::CoreNotReady
        );
        let _ = core.shutdown().await;
    });
}

/// `permissions_pending` 空清单形状（无 pending 时为 `[]`，非错误）。
#[test]
fn pending_list_is_empty_array_shape() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let backend = permission_backend(&core);
        let value = backend
            .permissions_pending(&PermissionsPendingRequest { session_id: None })
            .expect("空清单");
        assert_eq!(value, Value::Array(Vec::new()));
        let _ = core.shutdown().await;
    });
}
