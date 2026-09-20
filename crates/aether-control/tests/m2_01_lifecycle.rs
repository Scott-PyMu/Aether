//! M2-01 集成测试：会话状态机 / run 串行 / 幂等（含重启重放）/ ack 快路径 /
//! 120s 断流超时（时钟注入）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aether_control::{run_is_retryable, Clock, DegradeTrigger, ExecutorOutcome};
use aether_core::{ErrorInfo, RunStatus, SessionStatus};
use m2_support::{
    build_manager, create_session, manual_clock, message_count, reopen_core, system_clock,
    wait_for, wait_run_status, wait_session_status, ScriptedExecutor, TestCore,
};

fn client_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// DoD1（集成侧）：会话状态机全转移——创建 → 运行 → 回 idle → 关闭；终态拒绝新 run。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_state_machine_end_to_end() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "fsm").await;
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle
    );

    let ack = manager
        .send(&session.id, "hello", &client_id("fsm"))
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "执行器应被派发"
    );
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
        "run 应成功终态"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "run 终态后会话应回 idle"
    );

    // dispose：空闲会话 → completed（终态）。
    assert_eq!(
        manager.dispose(&session.id).await.unwrap(),
        SessionStatus::Completed
    );
    let error = manager
        .send(&session.id, "after-close", &client_id("closed"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "session_closed");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2：并行发送 3 条 → 1 执行 / 1 排队 / 第 3 条 `session_busy`（且第 3 条不落库）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sends_serialize_one_executing_one_queued_third_busy() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "serial").await;

    let first = manager
        .send(&session.id, "m1", &client_id("m1"))
        .await
        .unwrap();
    assert!(!first.queued && !first.duplicate);
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "第一条应立即派发执行"
    );

    let second = manager
        .send(&session.id, "m2", &client_id("m2"))
        .await
        .unwrap();
    assert!(second.queued, "第二条应进入等待队列");
    assert!(!second.duplicate);

    let error = manager
        .send(&session.id, "m3", &client_id("m3"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "session_busy", "第三条应回 session_busy");
    assert_eq!(
        message_count(&core.reads(), &session.id).await,
        2,
        "session_busy 的消息不得落库"
    );

    // 放行第一条 → 第二条自动提升执行。
    executor.release(1);
    assert!(
        wait_run_status(&manager, &first.run_id, RunStatus::Succeeded).await,
        "第一条应成功"
    );
    assert!(
        wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await,
        "等待队列应被提升并派发"
    );
    executor.release(1);
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Succeeded).await,
        "第二条应成功"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "两条 run 完成后会话应回 idle"
    );

    // 事件时间线（先日志后广播）：run.started/completed 按 seq 单调落库。
    let readback = core.pipeline.readback(&session.id, 0).await.unwrap();
    let types: Vec<&str> = readback
        .events
        .iter()
        .map(|event| event.event_type().as_str())
        .collect();
    for expected in [
        "session.created",
        "session.status_changed",
        "run.started",
        "run.completed",
    ] {
        assert!(types.contains(&expected), "缺少事件 {expected}: {types:?}");
    }
    assert!(
        readback
            .events
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq),
        "seq 必须严格单调"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD3：同一 `client_msg_id` 重发不重复；**重启核心后重放同值仍不重复**（ADR-005）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotent_send_survives_core_restart() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "idem").await;

    let key = client_id("idem");
    let first = manager.send(&session.id, "hi", &key).await.unwrap();
    assert!(!first.duplicate);
    assert!(
        wait_run_status(&manager, &first.run_id, RunStatus::Succeeded).await,
        "首次发送应完成"
    );

    let replay_same_core = manager.send(&session.id, "hi", &key).await.unwrap();
    assert!(replay_same_core.duplicate);
    assert_eq!(replay_same_core.message_id, first.message_id);
    assert_eq!(replay_same_core.run_id, first.run_id);
    assert_eq!(message_count(&core.reads(), &session.id).await, 1);

    // 核心重启：关管线/存储 → 同一数据目录重开 → 重放同值。
    let (temp, db_path) = core.shutdown().await;
    let core2 = reopen_core(temp, db_path).await;
    let executor2 = ScriptedExecutor::new(8);
    let manager2 = build_manager(
        &core2,
        system_clock(),
        executor2,
        aether_control::LifecycleConfig::default(),
    );
    assert_eq!(
        manager2.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle,
        "重启后会话仍可读"
    );
    let replay_after_restart = manager2.send(&session.id, "hi", &key).await.unwrap();
    assert!(replay_after_restart.duplicate, "重启后重放必须幂等命中");
    assert_eq!(replay_after_restart.message_id, first.message_id);
    assert_eq!(replay_after_restart.run_id, first.run_id);
    assert_eq!(
        message_count(&core2.reads(), &session.id).await,
        1,
        "重启重放不得新增消息行"
    );
    let stored = core2
        .reads()
        .message_by_client_msg_id(&session.id, &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.id, first.message_id);
    core2.pipeline.shutdown().await.unwrap();
    core2.storage.shutdown().await.unwrap();
}

/// DoD4：ack 快路径——消息与 run 行提交后立即返回，不等模型（时序断言）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_returns_after_commit_without_waiting_for_model() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "ack").await;
    let key = client_id("ack");

    let started = Instant::now();
    let ack = manager.send(&session.id, "fast", &key).await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "ack 必须立即返回（实测 {elapsed:?}；执行器被 0 许可阻塞）"
    );

    // 消息行已提交且可按幂等键读到。
    let stored = core
        .reads()
        .message_by_client_msg_id(&session.id, &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.id, ack.message_id);
    // run 行已提交（queued/running），模型尚未完成（无助手消息）。
    let run = manager.run(&ack.run_id).await.unwrap().unwrap();
    assert!(
        matches!(run.status, RunStatus::Queued | RunStatus::Running),
        "run 行必须已提交: {:?}",
        run.status
    );
    assert_eq!(message_count(&core.reads(), &session.id).await, 1);

    executor.release(1);
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
        "放行后 run 应完成"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD5：120s 无事件 → run failed（`run_stream_timeout`）且可重试（时钟注入）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_stream_timeout_fails_run_and_allows_retry() {
    let core = TestCore::open().await;
    let clock = manual_clock(1_000_000);
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "timeout").await;
    let ack = manager
        .send(&session.id, "hang", &client_id("hang"))
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);

    assert!(
        manager.watchdog_once().await.is_empty(),
        "未到 120s 不得判超时"
    );
    clock.advance(119_999);
    assert!(
        manager.watchdog_once().await.is_empty(),
        "119.999s 不得判超时"
    );

    clock.advance(1);
    let failed = manager.watchdog_once().await;
    assert_eq!(failed, vec![ack.run_id.clone()], "恰好 120s 判超时");

    let run = manager.run(&ack.run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("run_stream_timeout"));
    assert!(run_is_retryable(run.status), "超时 run 必须可重试");
    assert!(run.finished_at.is_some());
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "超时后会话回 idle 可继续送消息"
    );

    // 超时事件已广播（error.code = run_stream_timeout）。
    let readback = core.pipeline.readback(&session.id, 0).await.unwrap();
    let failed_event = readback
        .events
        .iter()
        .find(|event| event.event_type().as_str() == "run.failed")
        .expect("run.failed 事件必须落库");
    let payload = failed_event.payload.to_value().unwrap();
    assert_eq!(payload["error"]["code"], "run_stream_timeout");
    assert_eq!(payload["error"]["recoverable"], true);

    // 重试：新消息可正常受理（旧执行器阻塞任务迟到结果被忽略）。
    let retry = manager
        .send(&session.id, "retry", &client_id("retry"))
        .await
        .unwrap();
    assert!(!retry.duplicate);
    assert!(wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await);
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD5（活动重置）：任意事件到达重置断流计时。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_activity_resets_stream_timeout() {
    let core = TestCore::open().await;
    let clock = manual_clock(2_000_000);
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "activity").await;
    let ack = manager
        .send(&session.id, "active", &client_id("active"))
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);

    clock.advance(119_000);
    manager.touch_run(&ack.run_id).await;
    clock.advance(119_000);
    assert!(
        manager.watchdog_once().await.is_empty(),
        "活动事件必须重置断流计时（2×119s < 120s 间隔）"
    );
    clock.advance(1_000);
    let failed = manager.watchdog_once().await;
    assert_eq!(failed, vec![ack.run_id.clone()]);
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// 执行器失败终态：会话回 idle，run 状态与事件一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn executor_failure_reaches_terminal_state() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(2);
    executor.set_outcome(ExecutorOutcome::Failed {
        error: ErrorInfo {
            code: "model_error".to_owned(),
            message: "模拟失败".to_owned(),
            recoverable: true,
        },
    });
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "failure").await;
    let ack = manager
        .send(&session.id, "boom", &client_id("boom"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Failed).await,
        "执行器失败应落 run.failed"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "执行器失败后会话应回 idle"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// interrupt：取消执行中与等待 run；随后 dispose 进入终态；未知会话拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_cancels_active_and_waiting_then_dispose_closes() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "interrupt").await;
    let first = manager
        .send(&session.id, "m1", &client_id("int1"))
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);
    let second = manager
        .send(&session.id, "m2", &client_id("int2"))
        .await
        .unwrap();
    assert!(second.queued);

    let report = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(first.run_id.clone()));
    assert_eq!(report.cancelled_waiting_run, Some(second.run_id.clone()));
    assert!(
        wait_run_status(&manager, &first.run_id, RunStatus::Cancelled).await,
        "执行中 run 应 cancelled"
    );
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Cancelled).await,
        "等待 run 应 cancelled"
    );
    assert!(wait_session_status(&manager, &session.id, SessionStatus::Idle).await);

    // 中断后可继续送消息（M2-05 完整取消树的前置语义）。
    let third = manager
        .send(&session.id, "m3", &client_id("int3"))
        .await
        .unwrap();
    assert!(!third.queued);
    assert!(wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await);

    // dispose：有在途 run → cancelled 终态；随后新 run 拒绝。
    let status = manager.dispose(&session.id).await.unwrap();
    assert_eq!(status, SessionStatus::Cancelled);
    let error = manager
        .send(&session.id, "m4", &client_id("int4"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "session_closed");

    // 未知会话 → session_not_found（hydrate 失败路径）。
    let missing = aether_core::SessionId::new("01J0000000000000000000000Z").unwrap();
    let error = manager.send(&missing, "x", "c").await.unwrap_err();
    assert_eq!(error.code(), "session_not_found");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// 后台任务：事件监听重置断流计时 + 看门狗在 120s 后判超时（真实后台循环）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_tasks_track_events_and_watchdog_timeout() {
    let core = TestCore::open().await;
    let clock = manual_clock(3_000_000);
    let executor = ScriptedExecutor::new(0);
    let config = aether_control::LifecycleConfig {
        watchdog_tick: Duration::from_millis(20),
        ..aether_control::LifecycleConfig::default()
    };
    let manager = build_manager(&core, clock.clone(), executor.clone(), config);
    let spawned = manager.spawn_background(&tokio::runtime::Handle::current());
    assert_eq!(spawned, 3, "看门狗 + 事件监听 + 降级中断监听");
    let session = create_session(&manager, "bg").await;
    let ack = manager
        .send(&session.id, "bg", &client_id("bg"))
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);

    // 事件监听：落盘事件（带 run_id）重置活动时间。
    let event = serde_json::json!({
        "v": 1,
        "id": "01J00000000000000000000EV1",
        "session_id": session.id.as_str(),
        "run_id": ack.run_id.as_str(),
        "runtime_id": "mock",
        "seq": 0,
        "ts": clock.now_ms(),
        "type": "log",
        "payload": {"level": "info", "message": "活动"},
    });
    core.pipeline.submit(event).await.unwrap();
    // 等待后台事件监听消费（广播投递异步；确保 touch 发生在时钟推进之前）。
    tokio::time::sleep(Duration::from_millis(50)).await;
    clock.advance(119_000);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        manager.run(&ack.run_id).await.unwrap().unwrap().status != RunStatus::Failed,
        "事件活动应重置断流计时（后台看门狗持续巡检）"
    );

    // 超过 120s 无事件 → 后台看门狗自动判超时。
    clock.advance(2_000);
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Failed).await,
        "后台看门狗应判 120s 断流"
    );
    manager.shutdown_background().await;
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// 管线降级中断：在途 run 转 cancelled；降级期拒绝新 run（persist_degraded）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn degraded_interrupt_cancels_inflight_run_and_blocks_new_sends() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let spawned = manager.spawn_background(&tokio::runtime::Handle::current());
    assert_eq!(spawned, 3);
    let session = create_session(&manager, "degraded").await;
    let ack = manager
        .send(&session.id, "dg", &client_id("dg"))
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);

    let transitioned = core
        .pipeline
        .signal_degraded(DegradeTrigger::IntegrityFailure {
            detail: "故障注入".to_owned(),
        })
        .await
        .unwrap();
    assert!(transitioned);
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Cancelled).await,
        "降级中断监听应将在途 run 转 cancelled"
    );
    let error = manager
        .send(&session.id, "after", &client_id("after"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    manager.shutdown_background().await;
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
