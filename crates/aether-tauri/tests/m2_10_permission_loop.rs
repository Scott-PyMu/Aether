//! M2-10 集成测试：权限回环（设计 D9/D6）——适配器工具调用 → `permission.request`
//! 通知 → 核心权限网关（策略/审批 pending/300s 超时/审计）→ `permission.resolve`
//! 请求 → 适配器 `permission.resolved` + 工具终态。
//!
//! 覆盖 DoD（实施计划 v1.13 §3）：
//! 1. **基础回环（必须通过）**：文件类工具调用 100% 经 `permission.request` 回环，
//!    零直通（[`PermissionLoopProbe`] 计数断言：收到 = 决议 = 下发）；
//! 2. 异常路径：deny / 超时 deny / once / session 授权 / 重启后 pending 决议；
//! 3. 回环事件序列证据可导出归档（`AETHER_M2_10_EVIDENCE_DIR` 下的 JSON）。
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：回环仅约束适配器经线协议上报的工具调用；
//! 适配器进程内行为不经此门。
//!
//! 环境：真实 Mock 适配器进程经 `AETHER_MOCK_ADAPTER` 指定（由
//! `scripts/test/m2-10/verify-m2-10.mjs` 构建并设置；未设置时跳过，
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
    EventPipeline, ManualClock, PermissionConfig, PermissionService, PipelineConfig,
    StartupSelfCheckReport, StoreEventSource, StoreJournal,
};
use aether_core::{
    EventPayload, EventType, PermissionDecision, PermissionScope, PermissionStatus, Runtime,
    RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus, TokenUsage,
};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, StoreCommand, StoreRuntime, WriteQueueConfig};
use aether_tauri::permission_loop::PermissionServiceGate;
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
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m2-10 构建 Mock 后执行）"
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

/// 证据导出（DoD3）：`AETHER_M2_10_EVIDENCE_DIR` 存在时写 JSON；同时打印摘要行。
fn write_evidence(name: &str, value: &Value) {
    println!("[m2-10] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M2_10_EVIDENCE_DIR") else {
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
        // 同一分配（Arc 指针克隆）经 trait object 注入服务；测试侧仍可 `advance`。
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

    fn write(&self) -> aether_store::WriteQueue {
        self.storage.queue().clone()
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
        self.write()
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await
            .expect("runtimes 行");
        let session = Session {
            id: SessionId::new(session_id).unwrap(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: format!("m2-10-{session_id}"),
            status: SessionStatus::Idle,
            model: None,
            system_prompt: None,
            config: json!({}),
            token_usage: TokenUsage::default(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
        };
        self.write()
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

    /// 等待指定审计动作落库（回环入口审计在 pending 入队之后完成，重启前必须确认）。
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
        let readback = self
            .pipeline
            .readback(&SessionId::new(session_id).unwrap(), 0)
            .await
            .expect("补读");
        readback
            .events
            .iter()
            .map(|event| event.event_type().as_str().to_owned())
            .collect()
    }

    async fn session_events(&self, session_id: &str) -> Vec<aether_core::EventEnvelope> {
        self.pipeline
            .readback(&SessionId::new(session_id).unwrap(), 0)
            .await
            .expect("补读")
            .events
    }
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
    assert_eq!(hello.runtime.name, "mock");
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
            .create_session(Some("m2-10"), None, None)
            .await
            .expect("session.create")
            .session_id
    }

    async fn send_loop(&self, session_id: &str, target: &Path) -> String {
        self.client
            .send(
                session_id,
                &next_msg_id("m2-10-write"),
                &format!("permission-loop:{}", target.display()),
            )
            .await
            .expect("session.send ack")
            .run_id
    }

    async fn send_loop_read(&self, session_id: &str, target: &Path) -> String {
        self.client
            .send(
                session_id,
                &next_msg_id("m2-10-read"),
                &format!("permission-loop-read:{}", target.display()),
            )
            .await
            .expect("session.send ack")
            .run_id
    }

    async fn shutdown(&mut self) {
        // 先中止在途回环任务（释放网关等待者），再优雅关闭适配器进程。
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

// ===== DoD1：基础回环（必须通过）=====

#[test]
fn basic_loop_is_100_percent_and_zero_passthrough() {
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

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await,
            "fs.write 工作区内必须经回环进入待审批（ask）"
        );
        let pending_rows = core.reads().permissions_pending(None).await.unwrap();
        assert_eq!(pending_rows.len(), 1, "pending 必须已持久化");
        assert_eq!(pending_rows[0].status, PermissionStatus::Pending);
        assert_eq!(pending_rows[0].resource, "fs.write");
        let ticket = core.service.pending_list(None).first().unwrap().clone();
        assert_eq!(
            ticket.target.as_deref(),
            Some(target.to_string_lossy().as_ref()),
            "原始 target 原样上报（D9）"
        );

        // 探针：回环请求已收；UI 决议前不得有决议/下发（零直通的第一半）。
        let before = adapter.probe.snapshot();
        assert_eq!(before.requests_received, 1);
        assert_eq!(before.decisions, 0, "决议必须来自网关，不得预置");
        assert_eq!(before.resolutions_sent, 0);
        assert_eq!(
            adapter.client.permission_requests().await.len(),
            1,
            "适配器必须发 permission.request 通知（无旁路）"
        );

        // UI 决议：once allow。
        core.service
            .resolve(
                &ticket.request_id,
                PermissionDecision::Allow,
                Some(PermissionScope::Once),
            )
            .await
            .unwrap();
        let outcome = adapter
            .client
            .wait_run_outcome(&run_id, Duration::from_secs(10))
            .await
            .expect("run 必须到达终态");
        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "allow 后应 completed：{outcome:?}"
        );

        let types = adapter.client.run_event_types(&run_id).await;
        assert_eq!(
            types,
            vec![
                "run.started",
                "tool.call_started",
                "permission.resolved",
                "tool.call_completed",
                "run.completed"
            ],
            "回环事件序列"
        );
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        assert_eq!(snapshot.requests_received, 1);
        assert_eq!(snapshot.decisions, 1);
        assert_eq!(snapshot.resolutions_sent, 1);

        // 核心侧：permission.requested/resolved 落库 + 审计；pending 清空。
        assert_eq!(
            core.session_event_types(&session_id).await,
            vec!["permission.requested", "permission.resolved"]
        );
        let actions = core.audit_actions().await;
        for expected in ["permission.requested", "permission.resolved"] {
            assert!(actions.contains(&expected.to_owned()), "{actions:?}");
        }
        assert!(core.service.pending_list(None).is_empty());

        write_evidence(
            "dod1_basic_loop",
            &json!({
                "task": "M2-10 DoD1 基础回环",
                "session_id": session_id,
                "run_id": run_id,
                "adapter_event_sequence": types,
                "core_event_sequence": core.session_event_types(&session_id).await,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
                "pending_after": core.service.pending_list(None).len(),
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

// ===== DoD2：异常路径 =====

/// deny：UI 拒绝 → 适配器 `permission.resolved(deny)` + `tool.call_failed(denied)`。
#[test]
fn deny_path_resolves_and_fails_tool() {
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

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        let ticket = core.service.pending_list(None).first().unwrap().clone();
        core.service
            .resolve(&ticket.request_id, PermissionDecision::Deny, None)
            .await
            .unwrap();

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
        let resolved = payload_of(&events, &run_id, EventType::PermissionResolved).unwrap();
        match &resolved.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Deny);
                assert_eq!(payload.scope, None);
            }
            other => panic!("payload 类型不符：{other:?}"),
        }
        let failed = payload_of(&events, &run_id, EventType::ToolCallFailed).unwrap();
        match &failed.payload {
            EventPayload::ToolCallFailed(payload) => {
                assert_eq!(payload.error.code, "denied");
                assert!(!payload.error.recoverable);
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
            "dod2_deny",
            &json!({
                "task": "M2-10 DoD2 deny",
                "adapter_event_sequence": types,
                "tool_error": "denied",
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// 超时 deny：ManualClock +300s → 巡检 → deny 下发 + 审计 `permission.timeout`。
#[test]
fn timeout_deny_is_delivered_to_adapter() {
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

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        // 299.999s 不超时；恰好 +300.000s 判 deny（时钟注入，D9）。
        core.clock.advance(299_999);
        assert!(core.service.sweep_timeouts_once().await.is_empty());
        core.clock.advance(1);
        let timed_out = core.service.sweep_timeouts_once().await;
        assert_eq!(timed_out.len(), 1, "恰好 300s 必须判超时");

        let outcome = adapter
            .client
            .wait_run_outcome(&run_id, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        let types = adapter.client.run_event_types(&run_id).await;
        assert_eq!(types[2], "permission.resolved");
        assert_eq!(types[3], "tool.call_failed");
        let events = adapter.client.events().await;
        let resolved = payload_of(&events, &run_id, EventType::PermissionResolved).unwrap();
        match &resolved.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Deny);
            }
            other => panic!("payload 类型不符：{other:?}"),
        }
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        let actions = core.audit_actions().await;
        assert!(
            actions.contains(&"permission.timeout".to_owned()),
            "{actions:?}"
        );
        write_evidence(
            "dod2_timeout_deny",
            &json!({
                "task": "M2-10 DoD2 超时 deny",
                "adapter_event_sequence": types,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// once 与 session 授权：once 不跨请求；session 同 target 命中直接 allow（仍经回环）。
#[test]
fn once_and_session_grants_apply_through_loop() {
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

        // run1：once allow。
        let run1 = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        let ticket = core.service.pending_list(None).first().unwrap().clone();
        core.service
            .resolve(
                &ticket.request_id,
                PermissionDecision::Allow,
                Some(PermissionScope::Once),
            )
            .await
            .unwrap();
        assert!(matches!(
            adapter
                .client
                .wait_run_outcome(&run1, Duration::from_secs(10))
                .await,
            Some(RunOutcome::Completed { .. })
        ));

        // run2：同 target 再次 ask（once 不产生会话授权）→ session allow。
        let run2 = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await,
            "once 授权不得跨请求"
        );
        let ticket = core.service.pending_list(None).first().unwrap().clone();
        core.service
            .resolve(
                &ticket.request_id,
                PermissionDecision::Allow,
                Some(PermissionScope::Session),
            )
            .await
            .unwrap();
        assert!(matches!(
            adapter
                .client
                .wait_run_outcome(&run2, Duration::from_secs(10))
                .await,
            Some(RunOutcome::Completed { .. })
        ));

        // run3：同 target 命中会话级授权 → 无 pending，仍经回环（探针计数）。
        let decisions_before = adapter.probe.snapshot().decisions;
        let run3 = adapter.send_loop(&session_id, &target).await;
        let outcome = adapter
            .client
            .wait_run_outcome(&run3, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        assert!(
            wait_for(
                || adapter.probe.snapshot().decisions == decisions_before + 1,
                Duration::from_secs(5)
            )
            .await,
            "会话级授权路径同样经回环"
        );
        assert!(
            core.service.pending_list(None).is_empty(),
            "授权命中不产生 pending"
        );
        let types = adapter.client.run_event_types(&run3).await;
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
        let events = adapter.client.events().await;
        let resolved = payload_of(&events, &run3, EventType::PermissionResolved).unwrap();
        match &resolved.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Allow);
                assert_eq!(payload.scope, Some(PermissionScope::Session));
            }
            other => panic!("payload 类型不符：{other:?}"),
        }
        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        assert_eq!(snapshot.decisions, 3);
        let actions = core.audit_actions().await;
        assert!(
            actions.contains(&"permission.session_grant_hit".to_owned()),
            "{actions:?}"
        );
        write_evidence(
            "dod2_once_session",
            &json!({
                "task": "M2-10 DoD2 once/session 授权",
                "run_ids": [run1, run2, run3],
                "session_grant_run_sequence": types,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// 策略直决（fs.read）：工作区内 allow / 工作区外 deny，均经回环上报。
#[test]
fn read_policy_decisions_flow_through_loop() {
    let runtime = new_runtime();
    runtime.block_on(async {
        let core = TestCore::open().await;
        let Some(mut adapter) = launch(core.service.clone()).await else {
            return;
        };
        let session_id = adapter.open_session().await;
        core.insert_session(&session_id).await;
        let inside = core.workspace.path().join("readme.md");
        std::fs::write(&inside, b"hello").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("secret.txt");
        std::fs::write(&outside, b"top-secret").unwrap();

        // 工作区内：策略 allow（无 pending）。
        let run_in = adapter.send_loop_read(&session_id, &inside).await;
        let outcome = adapter
            .client
            .wait_run_outcome(&run_in, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        assert!(
            core.service.pending_list(None).is_empty(),
            "策略 allow 不进入审批"
        );
        let types_in = adapter.client.run_event_types(&run_in).await;
        assert_eq!(types_in[2], "permission.resolved");
        assert_eq!(types_in[3], "tool.call_completed");

        // 工作区外：策略 deny（无 pending）。
        let run_out = adapter.send_loop_read(&session_id, &outside).await;
        let outcome = adapter
            .client
            .wait_run_outcome(&run_out, Duration::from_secs(10))
            .await
            .expect("终态");
        assert!(matches!(outcome, RunOutcome::Completed { .. }));
        assert!(
            core.service.pending_list(None).is_empty(),
            "策略 deny 不进入审批"
        );
        let types_out = adapter.client.run_event_types(&run_out).await;
        assert_eq!(types_out[2], "permission.resolved");
        assert_eq!(types_out[3], "tool.call_failed");
        let events = adapter.client.events().await;
        let resolved = payload_of(&events, &run_out, EventType::PermissionResolved).unwrap();
        match &resolved.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Deny);
            }
            other => panic!("payload 类型不符：{other:?}"),
        }

        let snapshot = adapter.probe.snapshot();
        assert!(snapshot.zero_passthrough(), "{}", snapshot.summary());
        assert_eq!(snapshot.decisions, 2, "读/拒两条都必须经回环");
        let actions = core.audit_actions().await;
        assert!(actions.contains(&"permission.allowed_by_policy".to_owned()));
        assert!(actions.contains(&"permission.denied_by_policy".to_owned()));
        write_evidence(
            "dod2_policy_direct",
            &json!({
                "task": "M2-10 DoD2 策略直决（fs.read）",
                "allow_sequence": types_in,
                "deny_sequence": types_out,
                "probe": snapshot_evidence(&snapshot),
                "audit_actions": actions,
            }),
        );

        adapter.shutdown().await;
        let _ = core.shutdown().await;
    });
}

/// 重启后 pending 决议：pending 由真实回环产生；核心重启恢复后决议 + 审计；
/// 重复决议被拒（无重复语义）。
#[test]
fn pending_survives_core_restart_and_resolves() {
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

        let run_id = adapter.send_loop(&session_id, &target).await;
        assert!(
            wait_for(
                || !core.service.pending_list(None).is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        let pending_rows = core.reads().permissions_pending(None).await.unwrap();
        assert_eq!(pending_rows.len(), 1, "回环入口必须落库 pending");
        let request_id = pending_rows[0].request_id.clone().unwrap();
        // 回环入口审计（permission.requested）必须先落库，再模拟核心关闭。
        assert!(
            core.wait_audit_action("permission.requested", Duration::from_secs(5))
                .await,
            "回环入口审计必须落库"
        );

        // 核心关闭（适配器随应用退出）：中止在途回环任务 → 关闭管线/存储。
        adapter.shutdown().await;
        drop(adapter);
        let (temp, db_path, workspace, clock) = core.shutdown().await;

        // 重启核心：同一数据目录恢复 pending。
        let restarted = TestCore::reopen(temp, db_path, workspace, clock).await;
        let restored = restarted.service.restore_pending().await.unwrap();
        assert_eq!(restored, 1, "重启后待审批必须恢复");
        let ticket = restarted
            .service
            .pending_list(None)
            .first()
            .unwrap()
            .clone();
        assert_eq!(ticket.request_id, request_id);

        // 重启后决议：deny（用户可在 UI 拒绝遗留请求）。
        restarted
            .service
            .resolve(&ticket.request_id, PermissionDecision::Deny, None)
            .await
            .unwrap();
        assert!(restarted.service.pending_list(None).is_empty());
        let actions = restarted.audit_actions().await;
        assert!(
            actions.contains(&"permission.requested".to_owned()),
            "{actions:?}"
        );
        assert!(
            actions.contains(&"permission.resolved".to_owned()),
            "{actions:?}"
        );
        let types = restarted.session_event_types(&session_id).await;
        assert!(
            types.contains(&"permission.resolved".to_owned()),
            "{types:?}"
        );
        let events = restarted.session_events(&session_id).await;
        let resolved = events
            .iter()
            .find(|event| event.event_type() == EventType::PermissionResolved)
            .expect("重启后决议事件");
        match &resolved.payload {
            EventPayload::PermissionResolved(payload) => {
                assert_eq!(payload.decision, PermissionDecision::Deny);
            }
            other => panic!("payload 类型不符：{other:?}"),
        }

        // 重复决议 → permission_not_pending（无重复语义）。
        let error = restarted
            .service
            .resolve(&ticket.request_id, PermissionDecision::Allow, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "permission_not_pending");

        write_evidence(
            "dod2_restart_pending",
            &json!({
                "task": "M2-10 DoD2 重启后 pending 决议",
                "request_id": request_id,
                "restored": restored,
                "core_event_sequence_after_restart": types,
                "audit_actions_after_restart": actions,
                "duplicate_resolve": "permission_not_pending",
                "run_id": run_id,
            }),
        );
        let _ = restarted.shutdown().await;
    });
}
