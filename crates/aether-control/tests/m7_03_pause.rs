//! M7-03 集成测试：会话暂停/恢复（ADR-017；DoD1–DoD6）。
//!
//! 覆盖：
//! - DoD1 状态机：`running → paused → running` / `idle → paused → idle` /
//!   `waiting_permission → paused` 合法；`paused → paused`、非 `paused` resume、
//!   终态 pause 拒绝（`invalid_value` + 当前状态）；`session.status_changed` 每次转移发射；
//! - DoD2 暂停期不丢事件、seq 连续；恢复按 Mode R 语义重放（复用原输入消息、
//!   旧 run 保留审计；Mock 确定性断言）；
//! - DoD3 命令层能力门见 `aether-tauri/tests/m7_03_pause.rs`（本文件覆盖生命周期门禁）；
//! - DoD4 交互矩阵：paused 态 `send`/`run_retry` 拒绝（`session_paused`）、
//!   `interrupt` no-op、`dispose` → `cancelled`、看门狗不适用；
//! - DoD5 重启：`paused` 保留不收口、标记不跨重启、resume → `idle`；
//! - DoD6 审计 `session.pause`/`session.resume`（actor=user）+ 暂停统计。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aether_control::{LifecycleConfig, PauseNotifier};
use aether_core::{RunId, RunStatus, SessionId, SessionStatus};
use m2_support::{
    build_manager, create_session, insert_session_row_with_status, manual_clock, message_count,
    reopen_core, system_clock, wait_run_status, wait_session_status, ScriptedExecutor, TestCore,
};

fn client_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// `session.pause` 下发钩子替身（记录调用；可注入失败——best-effort 断言）。
struct FakePauseNotifier {
    calls: Mutex<Vec<SessionId>>,
    fail: bool,
}

impl FakePauseNotifier {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            fail,
        })
    }

    fn calls(&self) -> Vec<SessionId> {
        match self.calls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl PauseNotifier for FakePauseNotifier {
    fn notify_pause(
        &self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        match self.calls.lock() {
            Ok(mut guard) => guard.push(session_id),
            Err(poisoned) => poisoned.into_inner().push(session_id),
        }
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err("注入下发失败".to_owned())
            } else {
                Ok(())
            }
        })
    }
}

/// 会话内全部事件（按 seq 升序；覆盖上限足够的读法）。
async fn session_events(core: &TestCore, session_id: &SessionId) -> Vec<aether_core::EventEnvelope> {
    let mut events = core
        .reads()
        .events_latest(session_id, 1000)
        .await
        .unwrap();
    events.sort_by_key(|event| event.seq);
    events
}

/// seq 连续性断言（1..=N 无缺口、无重复）。
fn assert_seq_continuous(events: &[aether_core::EventEnvelope], label: &str) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq,
            (index + 1) as u64,
            "{label}：seq 缺口/重复（第 {index} 条 = {}）",
            event.seq
        );
    }
}

/// DoD1/DoD2/DoD6：在途 run 受控中断 → paused；恢复重放（Mode R；复用原输入消息）；
/// 审计与统计；seq 连续。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_running_session_interrupts_and_resume_replays() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let notifier = FakePauseNotifier::new(false);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    )
    .with_pause_notifier(notifier.clone());
    let session = create_session(&manager, "pause-running").await;

    // 在途 run（阻塞，不派发许可）。
    let ack = manager
        .send(&session.id, "long", &client_id("long"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Running).await,
        "run 应在执行中"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "会话应落库 running"
    );

    // ① 暂停。
    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.from, SessionStatus::Running);
    assert_eq!(report.to, SessionStatus::Paused);
    assert_eq!(report.interrupted_run, Some(ack.run_id.clone()));
    assert_eq!(report.waiting_kept, None);
    assert_eq!(report.pause_notify, Some(Ok(())));
    assert_eq!(notifier.calls(), vec![session.id.clone()], "下发一次 session.pause");
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Paused).await,
        "会话应落库 paused"
    );
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Cancelled).await,
        "在途 run 应收口 cancelled"
    );

    // 事件：run.cancelled(reason=user_pause) + session.status_changed(running→paused)。
    let events = session_events(&core, &session.id).await;
    assert_seq_continuous(&events, "暂停后");
    let cancelled_reason = events.iter().find_map(|event| match &event.payload {
        aether_core::EventPayload::RunCancelled(payload)
            if payload.run_id == ack.run_id =>
        {
            Some(payload.reason.clone())
        }
        _ => None,
    });
    assert_eq!(
        cancelled_reason,
        Some(Some("user_pause".to_owned())),
        "run.cancelled 应携带 reason=user_pause"
    );
    let change = events.iter().find_map(|event| match &event.payload {
        aether_core::EventPayload::SessionStatusChanged(payload)
            if payload.from == SessionStatus::Running && payload.to == SessionStatus::Paused =>
        {
            Some(())
        }
        _ => None,
    });
    assert!(change.is_some(), "必须发射 running → paused 状态事件");

    // DoD4：paused 态 send / run_retry 拒绝（session_paused）；interrupt no-op。
    let error = manager
        .send(&session.id, "再发一条", &client_id("paused-send"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "session_paused");
    assert!(error.to_string().contains("session_paused"));
    let error = manager.retry_run(&ack.run_id).await.unwrap_err();
    assert_eq!(error.code(), "session_paused");
    let interrupt = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(interrupt.interrupted_run, None, "paused 态 interrupt = no-op");
    assert_eq!(interrupt.cancelled_waiting_run, None);
    assert!(wait_session_status(&manager, &session.id, SessionStatus::Paused).await);

    // ② 恢复：有标记 → 重放（新 run；复用原输入消息；旧 run 保留审计）。
    let messages_before = message_count(&core.reads(), &session.id).await;
    let resume = manager.resume(&session.id).await.unwrap();
    assert_eq!(resume.from, SessionStatus::Paused);
    assert_eq!(resume.to, SessionStatus::Running);
    let replay_run = resume.run_id.clone().expect("有标记重放必须产生新 run");
    assert_ne!(replay_run, ack.run_id, "重放必须产生新 run");
    assert_eq!(
        resume.input_message_id,
        Some(ack.message_id.clone()),
        "重放复用原输入消息"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "恢复后会话应 running（先转移、后 spawn）"
    );
    // 放行旧任务（被摘除，迟到结果忽略）与重放任务。
    executor.release(2);
    assert!(
        wait_run_status(&manager, &replay_run, RunStatus::Succeeded).await,
        "重放 run 应成功"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "重放完成会话回 idle"
    );
    let old = manager.run(&ack.run_id).await.unwrap().expect("旧 run 行");
    assert_eq!(old.status, RunStatus::Cancelled, "旧 run 保留为 cancelled");
    assert_eq!(
        message_count(&core.reads(), &session.id).await,
        messages_before,
        "重放不新增用户消息行"
    );

    // DoD2：暂停/恢复全程 seq 连续（无缺口/重复）。
    let events = session_events(&core, &session.id).await;
    assert_seq_continuous(&events, "恢复后");
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        aether_core::EventPayload::SessionStatusChanged(payload)
            if payload.from == SessionStatus::Paused && payload.to == SessionStatus::Running
    )));

    // DoD6：审计（actor=user）与暂停统计。
    let audits = core.reads().audit_log(200).await.unwrap();
    let mine: Vec<_> = audits
        .iter()
        .filter(|row| row.session_id.as_deref() == Some(session.id.as_str()))
        .collect();
    let pause_audit = mine
        .iter()
        .find(|row| row.action == "session.pause")
        .expect("session.pause 审计行");
    assert_eq!(pause_audit.actor, "user");
    assert_eq!(pause_audit.result.as_deref(), Some("running→paused"));
    let detail = pause_audit.detail.as_deref().unwrap_or_default();
    assert!(detail.contains("user_pause"), "审计应含在途 run 收口摘要：{detail}");
    assert!(detail.contains("\"pause_notify\":\"ok\""), "审计应含下发结果：{detail}");
    let resume_audit = mine
        .iter()
        .find(|row| row.action == "session.resume")
        .expect("session.resume 审计行");
    assert_eq!(resume_audit.actor, "user");
    assert_eq!(resume_audit.result.as_deref(), Some("paused→running"));
    assert!(resume_audit
        .detail
        .as_deref()
        .unwrap_or_default()
        .contains("replay_of"));

    let stats = manager.pause_stats().await.unwrap();
    assert_eq!(stats.total_pauses, 1);
    assert_eq!(stats.total_resumes, 1);
    assert_eq!(stats.paused_sessions, 0);

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD3（生命周期侧）：下发失败不阻塞暂停收口（best-effort）；审计记录失败详情。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_notify_failure_does_not_block_pause() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let notifier = FakePauseNotifier::new(true);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    )
    .with_pause_notifier(notifier.clone());
    let session = create_session(&manager, "pause-notify-fail").await;
    let ack = manager
        .send(&session.id, "long", &client_id("notify-fail"))
        .await
        .unwrap();
    assert!(wait_run_status(&manager, &ack.run_id, RunStatus::Running).await);

    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.to, SessionStatus::Paused);
    assert_eq!(
        report.pause_notify,
        Some(Err("注入下发失败".to_owned())),
        "失败只记结果（不阻塞暂停）"
    );
    assert!(wait_session_status(&manager, &session.id, SessionStatus::Paused).await);

    let audits = core.reads().audit_log(200).await.unwrap();
    let pause_audit = audits
        .iter()
        .find(|row| row.action == "session.pause" && row.session_id.as_deref() == Some(session.id.as_str()))
        .expect("session.pause 审计行");
    assert!(pause_audit
        .detail
        .as_deref()
        .unwrap_or_default()
        .contains("注入下发失败"));

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD1/DoD4：`idle → paused`（纯门禁）与 `paused → idle`；非法转移拒绝；dispose 语义。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_pause_resume_and_illegal_transitions() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-idle").await;

    // idle → paused（无在途 run、无标记）。
    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.from, SessionStatus::Idle);
    assert_eq!(report.to, SessionStatus::Paused);
    assert_eq!(report.interrupted_run, None);
    assert_eq!(report.pause_notify, None, "未接线 = 跳过下发");

    // paused → paused 拒绝。
    let error = manager.pause(&session.id).await.unwrap_err();
    assert_eq!(error.code(), "invalid_value");
    assert!(error.to_string().contains("paused"));
    // 非 paused 态 resume 拒绝：先解除暂停再重试 resume。
    let resume = manager.resume(&session.id).await.unwrap();
    assert_eq!(resume.to, SessionStatus::Idle);
    assert_eq!(resume.run_id, None, "无标记解除不产生 run");
    assert!(resume.input_message_id.is_none());
    let error = manager.resume(&session.id).await.unwrap_err();
    assert_eq!(error.code(), "invalid_value");

    // 终态 pause 拒绝。
    manager.dispose(&session.id).await.unwrap();
    let error = manager.pause(&session.id).await.unwrap_err();
    assert_eq!(error.code(), "invalid_value");
    assert!(error.to_string().contains("completed"));

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD1/DoD4：`waiting_permission → paused` 直连边；paused 态 dispose → `cancelled`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_permission_direct_edge_and_dispose_cancels() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    // 直接落库构造 `waiting_permission` 会话（P0 无外部置位 API；状态机直连边用例）。
    let session =
        insert_session_row_with_status(&core, "01J0000000000000000000PAUSE", None, SessionStatus::WaitingPermission)
            .await;

    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.from, SessionStatus::WaitingPermission);
    assert_eq!(report.to, SessionStatus::Paused);
    let events = session_events(&core, &session.id).await;
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        aether_core::EventPayload::SessionStatusChanged(payload)
            if payload.from == SessionStatus::WaitingPermission && payload.to == SessionStatus::Paused
    )));

    // paused → dispose → cancelled（修正 P0 误算 completed 的路径）。
    let status = manager.dispose(&session.id).await.unwrap();
    assert_eq!(status, SessionStatus::Cancelled);
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Cancelled
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD1/DoD2：暂停保留等待队列（不提升、不取消）；恢复重放先于保留的 queued run。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_keeps_waiting_run_and_resume_precedes_it() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-waiting").await;

    let first = manager
        .send(&session.id, "first", &client_id("first"))
        .await
        .unwrap();
    let second = manager
        .send(&session.id, "second", &client_id("second"))
        .await
        .unwrap();
    assert!(second.queued, "第二条应进入会话等待槽");
    assert!(wait_run_status(&manager, &first.run_id, RunStatus::Running).await);

    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(first.run_id.clone()));
    assert_eq!(
        report.waiting_kept,
        Some(second.run_id.clone()),
        "等待队列保留不提升"
    );
    // 等待槽 run 保持 `queued`（不取消、不提升）。
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Queued).await,
        "queued run 在暂停期保持 queued"
    );

    // 恢复重放：新 run 先于保留的 queued run 设为 active。
    let resume = manager.resume(&session.id).await.unwrap();
    let replay = resume.run_id.clone().expect("重放 run");
    assert_eq!(resume.input_message_id, Some(first.message_id.clone()));
    executor.release(3); // 旧任务（迟到忽略）+ 重放 + 提升的 second。
    assert!(
        wait_run_status(&manager, &replay, RunStatus::Succeeded).await,
        "重放 run 应成功"
    );
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Succeeded).await,
        "等待槽 run 应在重放后提升并成功"
    );
    assert!(wait_session_status(&manager, &session.id, SessionStatus::Idle).await);

    // 事件 seq 连续。
    let events = session_events(&core, &session.id).await;
    assert_seq_continuous(&events, "等待槽场景");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD4：看门狗/断流超时对 paused 不适用（active 已摘除）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_does_not_touch_paused_session() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let clock = manual_clock(1_700_000_000_000);
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-watchdog").await;
    let ack = manager
        .send(&session.id, "long", &client_id("watchdog"))
        .await
        .unwrap();
    assert!(wait_run_status(&manager, &ack.run_id, RunStatus::Running).await);

    manager.pause(&session.id).await.unwrap();
    // 推进超过断流超时阈值后巡检：不得产生新的 run 终态（active 已摘除）。
    clock.advance(aether_control::RUN_STREAM_TIMEOUT_MS + 10_000);
    let failed = manager.watchdog_once().await;
    assert!(failed.is_empty(), "paused 会话不适用断流看门狗：{failed:?}");
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Paused
    );
    let run = manager.run(&ack.run_id).await.unwrap().expect("run 行");
    assert_eq!(run.status, RunStatus::Cancelled, "run 保持暂停收口的 cancelled");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD5：重启后 `paused` 保留（不收口）；标记不跨重启；resume → `idle`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_survives_restart_and_resume_returns_idle() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-restart").await;
    let ack = manager
        .send(&session.id, "long", &client_id("restart"))
        .await
        .unwrap();
    assert!(wait_run_status(&manager, &ack.run_id, RunStatus::Running).await);
    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(ack.run_id.clone()));

    // 模拟核心崩溃：关闭存储/管线（不执行收口），在既有数据目录重开。
    let (temp, db_path) = core.shutdown().await;
    let core = reopen_core(temp, db_path).await;
    let executor2 = ScriptedExecutor::new(4);
    let manager2 = build_manager(
        &core,
        system_clock(),
        executor2.clone(),
        LifecycleConfig::default(),
    );

    let report = manager2.reconcile_interrupted_runs().await.unwrap();
    assert!(
        !report.sessions_reset.contains(&session.id),
        "paused 会话不得被重启收口：{:?}",
        report.sessions_reset
    );
    assert_eq!(
        manager2.session_status(&session.id).await.unwrap(),
        SessionStatus::Paused,
        "paused 保留（不自动恢复/清理）"
    );

    // 标记不跨重启：resume 走 `paused → idle`（不重放）。
    let resume = manager2.resume(&session.id).await.unwrap();
    assert_eq!(resume.to, SessionStatus::Idle);
    assert_eq!(resume.run_id, None, "重启后无标记：不重放");
    assert_eq!(
        manager2.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD4：恢复受准入拒绝时保持 paused（可重试）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_is_rejected_while_persist_degraded_keeps_paused() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-degraded").await;
    let ack = manager
        .send(&session.id, "long", &client_id("degraded"))
        .await
        .unwrap();
    assert!(wait_run_status(&manager, &ack.run_id, RunStatus::Running).await);
    let report = manager.pause(&session.id).await.unwrap();
    assert_eq!(report.to, SessionStatus::Paused);

    assert!(
        core.pipeline
            .signal_degraded(aether_control::DegradeTrigger::WriteFailure {
                attempts: 3,
                last_error: "注入".to_owned(),
            })
            .await
            .unwrap()
    );
    let error = manager.resume(&session.id).await.unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Paused,
        "准入拒绝必须保持 paused"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD6：暂停统计（进程计数 + DB `paused` 事实源）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_stats_reflect_counters_and_paused_sessions() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let first = create_session(&manager, "stats-1").await;
    let second = create_session(&manager, "stats-2").await;

    let stats = manager.pause_stats().await.unwrap();
    assert_eq!((stats.total_pauses, stats.total_resumes, stats.paused_sessions), (0, 0, 0));

    manager.pause(&first.id).await.unwrap();
    manager.pause(&second.id).await.unwrap();
    let stats = manager.pause_stats().await.unwrap();
    assert_eq!((stats.total_pauses, stats.total_resumes, stats.paused_sessions), (2, 0, 2));

    manager.resume(&first.id).await.unwrap();
    let stats = manager.pause_stats().await.unwrap();
    assert_eq!((stats.total_pauses, stats.total_resumes, stats.paused_sessions), (2, 1, 1));

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2 补充：暂停不丢事件——暂停窗口内已有事件全部落库（以 run.cancelled 为界）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_does_not_drop_events_and_run_ids_are_stable() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "pause-seq").await;

    // 先完成一个 run（事件落库），再暂停。
    let first = manager
        .send(&session.id, "hello", &client_id("seq-first"))
        .await
        .unwrap();
    assert!(wait_run_status(&manager, &first.run_id, RunStatus::Succeeded).await);
    let before = session_events(&core, &session.id).await;
    assert_seq_continuous(&before, "暂停前");
    let max_before = before.last().map(|event| event.seq).unwrap_or(0);

    manager.pause(&session.id).await.unwrap();
    let after = session_events(&core, &session.id).await;
    assert_seq_continuous(&after, "暂停后");
    assert!(
        after.last().map(|event| event.seq).unwrap_or(0) > max_before,
        "暂停必须追加事件（状态转移），且旧事件不丢"
    );
    // 旧 run 行仍在（审计不删）。
    assert!(manager.run(&first.run_id).await.unwrap().is_some());
    let missing = RunId::new("01J00000000000000000000NOPE").unwrap();
    assert!(manager.run(&missing).await.unwrap().is_none());

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
