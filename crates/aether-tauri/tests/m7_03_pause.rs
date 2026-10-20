//! M7-03：`session_pause` / `session_resume` 命令层与真实夹具进程（ADR-017；DoD1–DoD3）。
//!
//! 环境：`AETHER_FIXTURE_BIN=<aether-adapter-fixture>`（缺省 → SKIP；`AETHER_REQUIRE_FIXTURE=1`
//! 时缺失视为失败——经 `pnpm verify:m7-03` 构建并注入）。
//!
//! 覆盖：
//! - DoD3 能力门：`pause_resume` 未声明（`--no-pause-resume` 夹具）→ 结构化错误
//!   `pause_unsupported`、零状态转移、零 `session.pause` 下发；
//! - DoD1/DoD2：在途 run 受控中断（`run.cancelled(reason=user_pause)`）→ `paused`；
//!   恢复重放（复用原输入消息；Mode R 原生上下文由夹具 `native_id` 保留）→ `running`；
//!   暂停期 `session_send` 拒绝（`session_paused`）；
//! - DoD5 相关：`paused → idle`（无标记）与非法状态矩阵（终态 pause/resume 拒绝）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{LifecycleConfig, SessionManager, SystemClock};
use aether_core::{RunId, RunStatus, SessionStatus};
use aether_store::{ReadPool, WriteQueue};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::IpcBackend;
use aether_tauri::ipc::dto::{SessionCreateRequest, SessionIdRequest, SessionSendRequest};
use aether_tauri::ipc::error::IpcError;
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

/// 未接线内层（SessionBackend 装饰器透传目标；本测试只走会话族命令）。
struct NotImplementedBackend;

impl IpcBackend for NotImplementedBackend {}

fn fixture_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_FIXTURE_BIN") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_FIXTURE").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_FIXTURE=1 但 AETHER_FIXTURE_BIN 未设置");
            }
            eprintln!("SKIP:AETHER_FIXTURE_BIN 未设置（由 pnpm verify:m7-03 构建并注入）");
            None
        }
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn spec_for(id: &str, fixture: &Path, log_path: &Path, no_pause_resume: bool) -> RuntimeSpec {
    let mut args = vec![
        "--mode".to_owned(),
        "session".to_owned(),
        "--session-log".to_owned(),
        log_path.to_string_lossy().to_string(),
    ];
    if no_pause_resume {
        args.push("--no-pause-resume".to_owned());
    }
    RuntimeSpec::with_fresh_token(
        RuntimeManifest::new(id, format!("Fixture {id}"), fixture.to_path_buf())
            .official(true)
            .with_args(args),
    )
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    #[allow(dead_code)]
    pipeline: aether_control::EventPipeline,
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    #[allow(dead_code)]
    supervisor: Arc<Supervisor>,
    log_path: PathBuf,
}

/// 两个运行时：`mock`（声明 `pause_resume`）与 `codex`（官方 id 占位，指向
/// `--no-pause-resume` 负向夹具——未声明能力路径）。
fn harness(fixture: &Path) -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let log_path = dir.path().join("session-log.jsonl");
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(dir.path(), &handle, Arc::new(StaticRuntimeSummaries::unwired()))
        .expect("启动核心（存储 + 管线）");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![
                spec_for("mock", fixture, &log_path, false),
                spec_for("codex", fixture, &log_path, true),
            ],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("构造监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(outcome.is_ready(), "{runtime_id} 预热必须 Ready：{outcome:?}");
    }
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    // M7-03/ADR-017：`session.pause` 下发钩子接线（生产接线同口径）。
    let notifier: Arc<dyn aether_control::PauseNotifier> = executor.clone();
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    )
    .with_pause_notifier(notifier);
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager.clone()),
        Some(executor),
        Some(reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle,
    ));
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        manager,
        backend,
        supervisor,
        log_path,
    }
}

fn create_session(h: &Harness, runtime_id: &str) -> String {
    let created = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some(runtime_id.to_owned()),
            title: format!("m7-03-{runtime_id}"),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
            preset_id: None,
        })
        .expect("创建会话");
    created["id"].as_str().expect("会话 id").to_owned()
}

fn send(h: &Harness, session_id: &str, text: &str, client_msg_id: &str) -> Value {
    h.backend
        .session_send(&SessionSendRequest {
            session_id: session_id.to_owned(),
            text: text.to_owned(),
            client_msg_id: client_msg_id.to_owned(),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect("发送消息")
}

fn pause(h: &Harness, session_id: &str) -> Result<Value, IpcError> {
    h.backend.session_pause(&SessionIdRequest {
        session_id: session_id.to_owned(),
    })
}

fn resume(h: &Harness, session_id: &str) -> Result<Value, IpcError> {
    h.backend.session_resume(&SessionIdRequest {
        session_id: session_id.to_owned(),
    })
}

fn session_status(h: &Harness, session_id: &str) -> SessionStatus {
    h.runtime
        .block_on(
            h.manager
                .session_status(&aether_core::SessionId::new(session_id).unwrap()),
        )
        .expect("会话状态")
}

fn run_status(h: &Harness, run_id: &str) -> Option<RunStatus> {
    h.runtime
        .block_on(h.manager.run(&RunId::new(run_id).unwrap()))
        .expect("run 读取")
        .map(|run| run.status)
}

fn session_events(h: &Harness, session_id: &str) -> Vec<aether_core::EventEnvelope> {
    let mut events = h
        .runtime
        .block_on(h.reads.events_latest(&aether_core::SessionId::new(session_id).unwrap(), 500))
        .expect("事件读取");
    events.sort_by_key(|event| event.seq);
    events
}

fn count_status_changes(h: &Harness, session_id: &str) -> usize {
    session_events(h, session_id)
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                aether_core::EventPayload::SessionStatusChanged(_)
            )
        })
        .count()
}

fn pause_dispatch_count(h: &Harness) -> usize {
    let Ok(text) = std::fs::read_to_string(&h.log_path) else {
        return 0;
    };
    text.lines()
        .filter(|line| line.contains("\"method\":\"session.pause\""))
        .count()
}

/// DoD3：未声明 `pause_resume` → `pause_unsupported`、零状态转移、零下发（不产生假暂停）。
#[test]
fn pause_unsupported_runtime_is_rejected_without_transition() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let h = harness(&fixture);
    let session_id = create_session(&h, "codex");
    assert_eq!(session_status(&h, &session_id), SessionStatus::Idle);
    let changes_before = count_status_changes(&h, &session_id);
    let dispatches_before = pause_dispatch_count(&h);

    let error = pause(&h, &session_id).expect_err("未声明能力必须拒绝");
    assert_eq!(error.code.as_str(), "pause_unsupported");
    assert!(
        error.message.contains("不支持暂停"),
        "UI 文案：{}",
        error.message
    );

    assert_eq!(
        session_status(&h, &session_id),
        SessionStatus::Idle,
        "能力门拒绝不得转移状态（不产生假暂停）"
    );
    assert_eq!(
        count_status_changes(&h, &session_id),
        changes_before,
        "零状态转移"
    );
    assert_eq!(pause_dispatch_count(&h), dispatches_before, "零 session.pause 下发");
    // resume 同样受能力门约束。
    let error = resume(&h, &session_id).expect_err("未声明能力 resume 也拒绝");
    assert_eq!(error.code.as_str(), "pause_unsupported");
}

/// DoD1/DoD2：在途 run 受控中断 → `paused`；`session.pause` 下发；恢复重放；
/// 暂停期 `session_send` 拒绝（`session_paused`）。
#[test]
fn pause_interrupts_and_resume_replays_through_fixture() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let h = harness(&fixture);
    let session_id = create_session(&h, "mock");

    // 在途 run（夹具 `long-once`：首次保持不返回终态；重放走正常完成）。
    let ack = send(&h, &session_id, "long-once", "01J8ZQ5R0N7W9Y8X6V4T2S0K30");
    let run_id = ack["run_id"].as_str().expect("run id").to_owned();
    assert!(
        wait_until(Duration::from_secs(10), || Some(RunStatus::Running) == run_status(&h, &run_id)),
        "run 应在执行中"
    );
    assert!(wait_until(Duration::from_secs(10), || {
        session_status(&h, &session_id) == SessionStatus::Running
    }));

    // ① 暂停（能力门通过 → 受控中断 → `paused` → `session.pause` 下发）。
    let report = pause(&h, &session_id).expect("暂停");
    assert_eq!(report["status"], "paused");
    assert_eq!(report["from"], "running");
    assert_eq!(report["interrupted_run"], run_id.as_str());
    assert_eq!(session_status(&h, &session_id), SessionStatus::Paused);
    assert!(
        wait_until(Duration::from_secs(10), || Some(RunStatus::Cancelled)
            == run_status(&h, &run_id)),
        "在途 run 应收口 cancelled"
    );
    assert!(
        wait_until(Duration::from_secs(10), || pause_dispatch_count(&h) >= 1),
        "必须下发 session.pause（best-effort；夹具记录）"
    );

    // 暂停期发送拒绝（稳定业务码 `session_paused`）。
    let error = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.clone(),
            text: "暂停中".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K31".to_owned(),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect_err("paused 态发送必须拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");
    assert!(error.message.contains("session_paused"));

    // ② 恢复：有暂停标记 → 重放（新 run 复用原输入消息）→ 夹具完成（2 delta + 终稿）。
    let report = resume(&h, &session_id).expect("恢复");
    assert_eq!(report["status"], "running");
    assert_eq!(report["from"], "paused");
    let replay_run = report["run_id"].as_str().expect("重放 run id").to_owned();
    assert_ne!(replay_run, run_id, "重放必须产生新 run");
    assert!(
        wait_until(Duration::from_secs(15), || {
            session_status(&h, &session_id) == SessionStatus::Idle
        }),
        "重放完成会话回 idle"
    );
    assert_eq!(run_status(&h, &replay_run), Some(RunStatus::Succeeded));

    // 事件：run.cancelled(user_pause) + 状态转移对（running→paused、paused→running、
    // running→idle；状态行先于事件落库，末次转移事件需等待落库可见）。
    let transitions_of = |h: &Harness| -> Vec<(SessionStatus, SessionStatus)> {
        session_events(h, &session_id)
            .iter()
            .filter_map(|event| match &event.payload {
                aether_core::EventPayload::SessionStatusChanged(payload) => {
                    Some((payload.from, payload.to))
                }
                _ => None,
            })
            .collect()
    };
    assert!(
        wait_until(Duration::from_secs(5), || transitions_of(&h)
            .contains(&(SessionStatus::Running, SessionStatus::Idle))),
        "必须落库 running → idle 状态事件"
    );
    let transitions = transitions_of(&h);
    assert!(transitions.contains(&(SessionStatus::Running, SessionStatus::Paused)));
    assert!(transitions.contains(&(SessionStatus::Paused, SessionStatus::Running)));
    let events = session_events(&h, &session_id);
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        aether_core::EventPayload::RunCancelled(payload)
            if payload.run_id.as_str() == run_id
                && payload.reason.as_deref() == Some("user_pause")
    )));
}

/// DoD1/DoD5：`idle → paused → idle`（无标记不重放）；非法状态矩阵。
#[test]
fn idle_pause_resume_cycle_and_illegal_state_matrix() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let h = harness(&fixture);
    let session_id = create_session(&h, "mock");

    // idle → paused（纯门禁；无在途 run）。
    let report = pause(&h, &session_id).expect("暂停 idle");
    assert_eq!(report["status"], "paused");
    assert_eq!(report["from"], "idle");
    assert_eq!(report["interrupted_run"], Value::Null);
    // 幂等错误：paused → paused 拒绝。
    let error = pause(&h, &session_id).expect_err("重复暂停拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");

    // paused → idle（无标记不产生 run）。
    let report = resume(&h, &session_id).expect("恢复");
    assert_eq!(report["status"], "idle");
    assert_eq!(report["run_id"], Value::Null);
    // 非 paused 态 resume 拒绝。
    let error = resume(&h, &session_id).expect_err("非 paused 恢复拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");
    assert!(error.message.contains("idle"));

    // 终态 pause 拒绝；会话不存在 → 稳定业务码前缀。
    h.backend
        .session_dispose(&SessionIdRequest {
            session_id: session_id.clone(),
        })
        .expect("关闭会话");
    let error = pause(&h, &session_id).expect_err("终态暂停拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");
    let error = pause(&h, "01J00000000000000000000MISS").expect_err("不存在的会话");
    assert_eq!(error.code.as_str(), "invalid_value");
    assert!(error.message.contains("session_not_found"));
}
