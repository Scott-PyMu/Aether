//! M7-01 集成测试（DoD0/DoD1/DoD2/DoD4/DoD7）：官方适配器热插拔状态机、事件发射
//! 链路、在途收口与重启矩阵。
//!
//! 宿主：`aether-adapter-fixture --mode session`（最小 D6 会话面；`--session-log`
//! 记录 create/send 调用，`text == "long"` 保持在途直至 `session.interrupt`/进程终止）。
//!
//! 运行：`AETHER_FIXTURE_BIN=<aether-adapter-fixture> cargo test -p aether-tauri
//! --test m7_01_hotplug`（`scripts/test/m7-01/verify-m7-01.mjs` 构建并设置；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_FIXTURE=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{
    AuditRecord, ProcessProbe, ResourceEvent, RuntimeManifest, RuntimeSpec, StatusChange,
    SupervisorObserver, SysinfoProbe,
};
use aether_control::{LifecycleConfig, SessionManager, SystemClock};
use aether_core::{RunId, RunStatus, SessionId};
use aether_store::ReadPool;
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::audit_bridge::StoreAuditObserver;
use aether_tauri::bindings::AetherEvent;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::event_bridge::{forward, BridgeMetrics, EventSink};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{RunRetryRequest, SessionCreateRequest, SessionSendRequest};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::runtime_control::{
    boot_supervisor_with_observer, run_supervisor_startup_with_prefs, RuntimeControl,
    SupervisorControl, RUNTIME_CONTROL_TIMEOUT,
};
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

fn fixture_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_FIXTURE_BIN") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_FIXTURE").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_FIXTURE=1 但 AETHER_FIXTURE_BIN 未设置");
            }
            eprintln!("SKIP：AETHER_FIXTURE_BIN 未设置（运行 pnpm verify:m7-01 构建夹具后执行）");
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

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return predicate();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 记录状态转移/审计的观察者（DoD0：每次合法转移均有回调）。
#[derive(Default)]
struct RecordingObserver {
    changes: Mutex<Vec<StatusChange>>,
    audits: Mutex<Vec<AuditRecord>>,
}

impl RecordingObserver {
    fn changes(&self) -> Vec<StatusChange> {
        match self.changes.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl SupervisorObserver for RecordingObserver {
    fn on_status_changed(&self, change: &StatusChange) {
        match self.changes.lock() {
            Ok(mut guard) => guard.push(change.clone()),
            Err(poisoned) => poisoned.into_inner().push(change.clone()),
        }
    }

    fn on_audit(&self, record: &AuditRecord) {
        match self.audits.lock() {
            Ok(mut guard) => guard.push(record.clone()),
            Err(poisoned) => poisoned.into_inner().push(record.clone()),
        }
    }
}

/// 同时转发到两个观察者（生产审计出口 + 测试记录）。
struct TeeObserver {
    first: Arc<dyn SupervisorObserver>,
    second: Arc<dyn SupervisorObserver>,
}

impl SupervisorObserver for TeeObserver {
    fn on_status_changed(&self, change: &StatusChange) {
        self.first.on_status_changed(change);
        self.second.on_status_changed(change);
    }

    fn on_audit(&self, record: &AuditRecord) {
        self.first.on_audit(record);
        self.second.on_audit(record);
    }

    fn on_resource_alert(&self, alert: &ResourceEvent) {
        self.first.on_resource_alert(alert);
        self.second.on_resource_alert(alert);
    }
}

/// 事件出口替身（`aether://event` 的生产 `emit` 替换；DoD0 广播端断言）。
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, String, String, u64)>>,
}

impl RecordingSink {
    fn snapshot(&self) -> Vec<(String, String, String, u64)> {
        match self.events.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: &AetherEvent) -> Result<(), String> {
        let record = (
            event.id.clone(),
            event.session_id.clone(),
            event.event_type.clone(),
            event.seq,
        );
        match self.events.lock() {
            Ok(mut guard) => guard.push(record),
            Err(poisoned) => poisoned.into_inner().push(record),
        }
        Ok(())
    }
}

fn spec_for(fixture: &Path, log_path: &Path) -> RuntimeSpec {
    RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Fixture Session", fixture)
            .official(true)
            .with_args([
                "--mode".to_owned(),
                "session".to_owned(),
                "--session-log".to_owned(),
                log_path.to_string_lossy().to_string(),
            ]),
    )
}

fn count_audit_rows(runtime: &tokio::runtime::Runtime, reads: &ReadPool, action: &str) -> u64 {
    let action = action.to_owned();
    runtime
        .block_on(reads.with_connection(move |connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM audit_log WHERE action = ?1",
                    [action.as_str()],
                    |row| row.get(0),
                )
                .map_err(aether_store::StoreError::from)?;
            Ok(u64::try_from(count).unwrap_or(0))
        }))
        .unwrap_or(0)
}

fn harness_block_on<T>(runtime: &tokio::runtime::Runtime, future: impl std::future::Future<Output = T>) -> T {
    runtime.block_on(future)
}

/// DoD0（事件链路）+ DoD1（状态机）+ DoD7（启动矩阵）：
/// `enabled=0` 不预热且置 `disabled + user_disabled`；转移全集逐次广播
/// `runtime.status_changed`（先日志后广播）；审计动作保留并存。
#[test]
fn transitions_emit_persisted_events_and_startup_matrix_holds() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let log_path = dir.path().join("session-log.jsonl");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    let boot: CoreBoot =
        boot_core_full(&data_dir, &handle, Arc::new(StaticRuntimeSummaries::unwired()))
            .expect("核心启动");

    // 事件桥替身（与生产同一 forward 循环；订阅先于预热）。
    let sink = Arc::new(RecordingSink::default());
    let metrics = Arc::new(BridgeMetrics::default());
    let receiver = boot.pipeline.subscribe();
    handle.spawn(forward(receiver, sink.clone(), Arc::clone(&metrics)));

    let recording = Arc::new(RecordingObserver::default());
    let audit_observer = Arc::new(
        StoreAuditObserver::new(boot.write.clone(), handle.clone())
            .with_pipeline(boot.pipeline.clone()),
    );
    let observer: Arc<dyn SupervisorObserver> = Arc::new(TeeObserver {
        first: audit_observer,
        second: recording.clone(),
    });
    let supervisor = Arc::new(
        boot_supervisor_with_observer(
            vec![spec_for(&fixture, &log_path)],
            Some(&dir.path().join("adapters.json")),
            observer,
        )
        .expect("监督器"),
    );

    // ===== DoD7 启动矩阵：enabled=0 → cold → disabled(user_disabled)，不 spawn =====
    let mut prefs = BTreeMap::new();
    prefs.insert("mock".to_owned(), false);
    let startup =
        run_supervisor_startup_with_prefs(&supervisor, &handle, &prefs).expect("启动序列尾段");
    assert!(matches!(
        startup.warmups[0].1,
        aether_adapters::supervisor::StartOutcome::Disabled { .. }
    ));
    let runtime_mock = supervisor.get("mock").expect("注册表");
    assert_eq!(
        harness_block_on(&runtime, runtime_mock.status()),
        aether_core::RuntimeStatus::Disabled
    );
    assert_eq!(
        harness_block_on(&runtime, runtime_mock.status_reason()).map(|reason| reason.as_str()),
        Some("user_disabled")
    );
    assert!(
        !harness_block_on(&runtime, runtime_mock.is_running()),
        "enabled=0 不得 spawn 进程"
    );
    // disabled 上再 disable → DisableNotAllowed（命令矩阵）。
    let error = harness_block_on(&runtime, supervisor.runtime_disable("mock")).unwrap_err();
    assert!(matches!(
        error,
        aether_adapters::supervisor::SupervisorError::DisableNotAllowed { .. }
    ));

    // ===== DoD1 enable：disabled → cold → starting → ready（不重启应用） =====
    let outcome = harness_block_on(&runtime, supervisor.runtime_enable("mock"));
    assert!(
        outcome.expect("enable").is_ready(),
        "user_disabled 上 enable 必须可用（ADR-015）"
    );
    assert!(harness_block_on(&runtime, runtime_mock.is_running()));

    // ===== DoD1/2 disable：先转移 disabled(user_disabled)、后受控终止 =====
    let outcome = harness_block_on(&runtime, supervisor.runtime_disable("mock")).expect("disable");
    assert!(outcome.terminated, "ready 上禁用必须终止进程");
    assert_eq!(outcome.reason.as_str(), "user_disabled");

    // 转移序列（观察者回调）：cold→disabled（启动矩阵）、disabled→cold→starting→ready、
    // ready→disabled——共 5 次，逐次有回调。
    let changes = recording.changes();
    let transitions: Vec<(String, String)> = changes
        .iter()
        .map(|change| (change.from.as_str().to_owned(), change.to.as_str().to_owned()))
        .collect();
    assert_eq!(
        transitions,
        vec![
            ("cold".to_owned(), "disabled".to_owned()),
            ("disabled".to_owned(), "cold".to_owned()),
            ("cold".to_owned(), "starting".to_owned()),
            ("starting".to_owned(), "ready".to_owned()),
            ("ready".to_owned(), "disabled".to_owned()),
        ]
    );

    // ===== DoD0 事件链路：每次合法转移一条 runtime.status_changed（先日志后广播）=====
    let event_session = SessionId::new("runtime:mock").unwrap();
    let persisted = wait_until(Duration::from_secs(30), || {
        harness_block_on(&runtime, boot.reads.event_count(&event_session)).unwrap_or(0) == 5
    });
    assert!(persisted, "5 次合法转移必须各持久化一条事件");
    let events = harness_block_on(&runtime, boot.reads.events_page(&event_session, None, 50))
        .expect("读事件");
    assert_eq!(events.len(), 5);
    for (index, envelope) in events.iter().enumerate() {
        assert_eq!(envelope.seq, (index as u64) + 1, "合成归段 seq 独立单调");
        assert_eq!(envelope.event_type().as_str(), "runtime.status_changed");
        assert_eq!(envelope.runtime_id.as_str(), "mock");
        assert_eq!(envelope.session_id.as_str(), "runtime:mock");
    }
    let payloads: Vec<Value> = events
        .iter()
        .map(|envelope| envelope.payload.to_value().expect("payload"))
        .collect();
    assert_eq!(payloads[0]["from"], "cold");
    assert_eq!(payloads[0]["to"], "disabled");
    assert_eq!(payloads[0]["reason"], "user_disabled");
    assert_eq!(payloads[4]["from"], "ready");
    assert_eq!(payloads[4]["to"], "disabled");
    assert_eq!(payloads[4]["reason"], "user_disabled");

    // 广播端：`aether://event` 收到同样的 5 条（且都已落盘 = 先日志后广播）。
    let broadcasted = wait_until(Duration::from_secs(30), || {
        sink.snapshot()
            .iter()
            .filter(|(_, session, event_type, _)| {
                session == "runtime:mock" && event_type == "runtime.status_changed"
            })
            .count()
            >= 5
    });
    assert!(broadcasted, "事件必须经桥接广播（aether://event 出口）");
    let persisted_ids: Vec<&str> = events.iter().map(|envelope| envelope.id.as_str()).collect();
    for (id, session, event_type, _) in sink.snapshot() {
        if session == "runtime:mock" && event_type == "runtime.status_changed" {
            assert!(
                persisted_ids.contains(&id.as_str()),
                "广播事件必须先落盘（D4 先日志后广播）：{id}"
            );
        }
    }

    // 审计动作保留：5 条 status_changed 审计 + 1 条终止（terminated）。
    let audit_ok = wait_until(Duration::from_secs(30), || {
        count_audit_rows(&runtime, &boot.reads, "runtime.status_changed") == 5
            && count_audit_rows(&runtime, &boot.reads, "runtime.terminated") >= 1
    });
    assert!(audit_ok, "状态转移与终止审计必须落库（事件与审计并存）");

    harness_block_on(&runtime, boot.pipeline.shutdown()).expect("管线关闭");
    harness_block_on(&runtime, supervisor.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = harness_block_on(&runtime, storage.shutdown());
    }
}

/// DoD2：在途确认门（`runtime_busy`）→ 确认后活跃 run `adapter_disconnected`/
/// `cli_exit`、queued run `adapter_unavailable`（均 recoverable）→ 重启用后
/// `run_retry` 可重放；隔离断言（其他运行时不受影响）；禁用后不可创建会话。
#[test]
fn disable_confirmation_closes_in_flight_runs_and_retry_replays() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let log_path = dir.path().join("session-log.jsonl");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    let boot: CoreBoot =
        boot_core_full(&data_dir, &handle, Arc::new(StaticRuntimeSummaries::unwired()))
            .expect("核心启动");
    let audit_observer = Arc::new(StoreAuditObserver::new(
        boot.write.clone(),
        handle.clone(),
    ));
    // 两个官方运行时（mock + codex）：禁用 mock 不得影响 codex（隔离断言）。
    let codex_spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("codex", "Fixture Codex", &fixture)
            .official(true)
            .with_args(["--mode".to_owned(), "session".to_owned()]),
    );
    let supervisor = Arc::new(
        boot_supervisor_with_observer(
            vec![spec_for(&fixture, &log_path), codex_spec],
            Some(&dir.path().join("adapters.json")),
            audit_observer,
        )
        .expect("监督器"),
    );
    let startup =
        run_supervisor_startup_with_prefs(&supervisor, &handle, &BTreeMap::new()).expect("启动");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(outcome.is_ready(), "{runtime_id} 预热必须 Ready：{outcome:?}");
    }
    let codex_pid_before = harness_block_on(
        &runtime,
        supervisor.get("codex").unwrap().current_pid(),
    );

    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        boot.pipeline.clone(),
        boot.reads.clone(),
        boot.write.clone(),
        handle.clone(),
        None,
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        boot.write.clone(),
        boot.reads.clone(),
        boot.pipeline.clone(),
        executor.clone(),
    );
    harness_block_on(&runtime, manager.reconcile_interrupted_runs()).expect("重启重建");
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager.clone()),
        Some(executor.clone()),
        Some(boot.reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle.clone(),
    ));
    let control = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    )
    .with_runtime_store(boot.reads.clone(), boot.write.clone());

    // ===== 建立活跃 run（long）+ queued run =====
    let created = backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some("mock".to_owned()),
            title: "M7-01 在途".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
            preset_id: None,
        })
        .expect("session_create");
    let session_id =
        SessionId::new(created["id"].as_str().expect("会话 id").to_owned()).unwrap();
    let active = backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "long".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K11".to_owned(),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect("session_send(long)");
    let active_run = RunId::new(active["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_until(Duration::from_secs(30), || {
            harness_block_on(&runtime, boot.reads.run(&active_run))
                .ok()
                .flatten()
                .map(|run| run.status)
                == Some(RunStatus::Running)
        }),
        "long run 必须在执行中"
    );
    // 屏障：等活跃 run 的 `run.started` 落盘——证明执行器已建立适配器会话、
    // `session.send` 已被适配器受理（此后断连必然经 `wait_run_outcome` 收口
    // `adapter_disconnected`，消除「执行器尚未到达适配器」竞态）。
    let started = wait_until(Duration::from_secs(30), || {
        harness_block_on(&runtime, boot.reads.events_page(&session_id, None, 200))
            .map(|events| {
                events.iter().any(|envelope| {
                    matches!(
                        &envelope.payload,
                        aether_core::EventPayload::RunStarted(payload)
                            if payload.run_id == active_run
                    )
                })
            })
            .unwrap_or(false)
    });
    assert!(
        started,
        "活跃 run 的 run.started 必须落盘（执行器已存活于适配器会话）"
    );
    let queued = backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "queued".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K12".to_owned(),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect("session_send(queued)");
    assert_eq!(queued["queued"], true, "第二条消息必须排队");
    let queued_run = RunId::new(queued["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || {
            harness_block_on(&runtime, boot.reads.active_work_counts())
                .map(|counts| counts.get("mock").copied().unwrap_or(0) >= 3)
                .unwrap_or(false)
        }),
        "在途计数必须 ≥3（running 会话 + 活跃 run + queued run）"
    );

    // ===== 未确认 → runtime_busy（不转移、不写偏好）=====
    // 在途计数口径（ADR-015 §2.1#2 读路径；与 `runtimes_list.active_sessions` 同源）：
    // 符合条件的会话数 + run 数之和 = 1（running 会话）+ 1（活跃 run）+ 1（queued run）。
    let error = control.disable("mock", false).expect_err("必须在途确认门");
    assert_eq!(error.code, IpcErrorCode::RuntimeBusy);
    assert_eq!(error.field.as_deref(), Some("confirm_terminate"));
    assert!(
        error.message.contains("3 个在途"),
        "runtime_busy 必须附计数：{}",
        error.message
    );
    assert_eq!(
        harness_block_on(&runtime, supervisor.get("mock").unwrap().status()),
        aether_core::RuntimeStatus::Ready,
        "未确认不得转移"
    );
    let preferences = harness_block_on(&runtime, boot.reads.runtime_preferences()).expect("偏好");
    assert_eq!(preferences.get("mock"), Some(&true), "未确认不得写偏好");

    // ===== 确认 → 偏好先写 → disabled+user_disabled → 受控终止 =====
    let report = control.disable("mock", true).expect("确认后禁用");
    assert_eq!(report["runtime_id"], "mock");
    assert_eq!(report["outcome"], "disabled");
    assert_eq!(report["status"], "disabled");
    assert_eq!(report["status_reason"], "user_disabled");
    assert!(
        report["detail"].as_str().unwrap_or("").contains("在途计数=3"),
        "回执 detail 必须含在途计数：{report}"
    );
    let preferences = harness_block_on(&runtime, boot.reads.runtime_preferences()).expect("偏好");
    assert_eq!(preferences.get("mock"), Some(&false), "确认后偏好必须先写 0");
    assert!(count_audit_rows(&runtime, &boot.reads, "runtime.disable") >= 1, "禁用动作审计");

    // 活跃 run 收口：adapter_disconnected / cli_exit（recoverable）。
    let closed = wait_until(Duration::from_secs(30), || {
        harness_block_on(&runtime, boot.reads.run(&active_run))
            .ok()
            .flatten()
            .map(|run| run.status)
            == Some(RunStatus::Failed)
    });
    assert!(closed, "活跃 run 必须收口 failed");
    let active_row = harness_block_on(&runtime, boot.reads.run(&active_run))
        .unwrap()
        .unwrap();
    let active_code = active_row.error.clone().unwrap_or_default();
    assert!(
        matches!(active_code.as_str(), "adapter_disconnected" | "cli_exit"),
        "活跃 run 收口码必须为 adapter_disconnected/cli_exit：{active_code}"
    );

    // queued run 收口：队列推进启动时命中既有 run 启动拒绝 adapter_unavailable。
    let queued_closed = wait_until(Duration::from_secs(30), || {
        harness_block_on(&runtime, boot.reads.run(&queued_run))
            .ok()
            .flatten()
            .map(|run| run.status)
            == Some(RunStatus::Failed)
    });
    assert!(queued_closed, "queued run 必须收口 failed");
    let queued_row = harness_block_on(&runtime, boot.reads.run(&queued_run))
        .unwrap()
        .unwrap();
    assert_eq!(
        queued_row.error.as_deref(),
        Some("adapter_unavailable"),
        "queued run 收口码必须复用 adapter_unavailable（ADR-015 决策 3）"
    );

    // 隔离断言：codex 不受影响（仍 ready 且进程未变）。
    assert_eq!(
        harness_block_on(&runtime, supervisor.get("codex").unwrap().status()),
        aether_core::RuntimeStatus::Ready
    );
    assert_eq!(
        harness_block_on(&runtime, supervisor.get("codex").unwrap().current_pid()),
        codex_pid_before
    );

    // 被禁用运行时不可创建会话（既有 disabled 语义）。
    // M7-05 门禁修复（设计驱动，留痕）：M7-02 会话创建路径的禁用文案强化为
    // 「已被用户禁用（enabled=0）」；断言改锚定语义稳定子串（"禁用" + "enabled=0"）。
    let error = backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some("mock".to_owned()),
            title: "不应创建".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
            preset_id: None,
        })
        .expect_err("disabled 运行时不得创建会话");
    assert!(
        error.message.contains("禁用") && error.message.contains("enabled=0"),
        "错误必须说明禁用：{}",
        error.message
    );

    // ===== 重启用 → run_retry 重放 queued run（Mode R/N，ADR-005）=====
    let report = control.enable("mock").expect("user_disabled 上 enable 必须可用");
    assert_eq!(report["outcome"], "ready");
    let preferences = harness_block_on(&runtime, boot.reads.runtime_preferences()).expect("偏好");
    assert_eq!(preferences.get("mock"), Some(&true), "启用成功必须写偏好 1");
    assert!(count_audit_rows(&runtime, &boot.reads, "runtime.enable") >= 1, "启用动作审计");

    let retry = backend
        .run_retry(&RunRetryRequest {
            run_id: queued_run.as_str().to_owned(),
        })
        .expect("重启用后 run_retry 必须可重放");
    let retry_run = RunId::new(retry["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert_ne!(retry_run, queued_run, "重放产生新 run");
    assert!(
        wait_until(Duration::from_secs(30), || {
            harness_block_on(&runtime, boot.reads.run(&retry_run))
                .ok()
                .flatten()
                .map(|run| run.status)
                == Some(RunStatus::Succeeded)
        }),
        "重放 run 应成功（临时目录日志：{log_path:?}）"
    );

    harness_block_on(&runtime, boot.pipeline.shutdown()).expect("管线关闭");
    harness_block_on(&runtime, supervisor.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = harness_block_on(&runtime, storage.shutdown());
    }
}

/// DoD4：连续 10 次热插拔循环——每次禁用后进程整树退出（无孤儿）、台账不残留；
/// 循环结束内存 RSS 增量与（Windows）句柄数增量受控（脚本化阈值）。
#[test]
fn ten_hotplug_cycles_leave_no_orphans_and_bounded_growth() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = TempDir::new().expect("临时目录");
    let ledger_path = dir.path().join("adapters.json");
    let log_path = dir.path().join("session-log.jsonl");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    let spec = spec_for(&fixture, &log_path);
    let supervisor = Arc::new(
        aether_tauri::runtime_control::boot_supervisor(
            vec![spec],
            Some(&ledger_path),
        )
        .expect("监督器"),
    );
    let startup =
        run_supervisor_startup_with_prefs(&supervisor, &handle, &BTreeMap::new()).expect("启动");
    assert!(startup.warmups[0].1.is_ready());
    let runtime_mock = supervisor.get("mock").expect("注册表");
    let probe = SysinfoProbe::new();

    let rss_before = process_rss_bytes(std::process::id());
    let handles_before = handle_count(std::process::id());

    let mut cycles = Vec::new();
    for cycle in 0..10 {
        let pid_before = harness_block_on(&runtime, runtime_mock.current_pid());
        assert!(pid_before.is_some(), "第 {cycle} 轮应持有运行进程");
        let outcome = harness_block_on(&runtime, supervisor.runtime_disable("mock"))
            .unwrap_or_else(|error| panic!("第 {cycle} 轮 disable 失败：{error}"));
        assert!(outcome.terminated, "第 {cycle} 轮必须终止进程");
        assert!(
            !harness_block_on(&runtime, runtime_mock.is_running()),
            "第 {cycle} 轮禁用后不得残留进程槽位"
        );
        // 进程真实退出（无孤儿）与台账清理。
        let pid = pid_before.unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || !probe.facts(pid).alive),
            "第 {cycle} 轮 PID {pid} 必须退出（无孤儿）"
        );
        let ledger_text = std::fs::read_to_string(&ledger_path).unwrap_or_default();
        let records: Vec<Value> = serde_json::from_str(&ledger_text).unwrap_or_default();
        assert!(
            records.is_empty(),
            "第 {cycle} 轮禁用后台账不得残留：{ledger_text}"
        );

        let outcome = harness_block_on(&runtime, supervisor.runtime_enable("mock"))
            .unwrap_or_else(|error| panic!("第 {cycle} 轮 enable 失败：{error}"));
        assert!(outcome.is_ready(), "第 {cycle} 轮启用必须 ready：{outcome:?}");
        let pid_after = harness_block_on(&runtime, runtime_mock.current_pid());
        assert!(
            pid_after.is_some() && pid_after != pid_before,
            "第 {cycle} 轮启用必须产生新进程"
        );
        cycles.push((pid_before, pid_after));
    }

    // 收口：最后一轮禁用后无进程残留。
    let _ = harness_block_on(&runtime, supervisor.runtime_disable("mock"));
    assert!(!harness_block_on(&runtime, runtime_mock.is_running()));

    let rss_after = process_rss_bytes(std::process::id());
    let handles_after = handle_count(std::process::id());
    let rss_growth = rss_after.saturating_sub(rss_before);
    eprintln!(
        "[m7-01] 10 轮热插拔：RSS {rss_before}B → {rss_after}B（增量 {rss_growth}B）；\
         句柄 {handles_before:?} → {handles_after:?}"
    );
    // 内存泄漏阈值（含适配器进程页缓存扰动裕量；实测值随证据归档）。
    assert!(
        rss_growth < 256 * 1024 * 1024,
        "10 轮循环 RSS 增量必须 <256MiB（实际 {rss_growth}B）"
    );
    if let (Some(before), Some(after)) = (handles_before, handles_after) {
        let growth = after.saturating_sub(before);
        assert!(
            growth <= 256,
            "10 轮循环句柄增量必须 ≤256（实际 {growth}：{before} → {after}）"
        );
    }

    // 证据写出（verify 脚本注入 AETHER_M7_01_EVIDENCE_DIR；缺省仅控制台输出）。
    if let Ok(dir) = std::env::var("AETHER_M7_01_EVIDENCE_DIR") {
        let evidence = serde_json::json!({
            "task": "M7-01",
            "case": "ten_hotplug_cycles",
            "cycles": cycles
                .iter()
                .map(|(before, after)| serde_json::json!({
                    "pid_before": before,
                    "pid_after": after,
                }))
                .collect::<Vec<_>>(),
            "rss_before_bytes": rss_before,
            "rss_after_bytes": rss_after,
            "rss_growth_bytes": rss_growth,
            "rss_threshold_bytes": 256u64 * 1024 * 1024,
            "handles_before": handles_before,
            "handles_after": handles_after,
            "handles_threshold": 256,
        });
        let path = std::path::Path::new(&dir).join("ten-hotplug-cycles.json");
        match serde_json::to_string_pretty(&evidence) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&path, text) {
                    eprintln!("[m7-01] 证据写出失败（仅诊断）：{error}");
                }
            }
            Err(error) => eprintln!("[m7-01] 证据序列化失败（仅诊断）：{error}"),
        }
    }

    harness_block_on(&runtime, supervisor.shutdown_all());
}

/// 自身进程 RSS（sysinfo 0.30：字节）。
fn process_rss_bytes(pid: u32) -> u64 {
    use sysinfo::{Pid, ProcessRefreshKind, System};
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessRefreshKind::new().with_memory());
    system
        .process(Pid::from_u32(pid))
        .map(sysinfo::Process::memory)
        .unwrap_or(0)
}

/// 进程句柄数（Windows：`Get-Process`；其他平台无此口径 → `None`）。
#[cfg(windows)]
fn handle_count(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).HandleCount"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(not(windows))]
fn handle_count(_pid: u32) -> Option<u64> {
    None
}
