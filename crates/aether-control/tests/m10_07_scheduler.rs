//! M10-07/ADR-037 调度器集成测试：优先级/公平性/超时/自动重试/队列可观测/10 并发。
//!
//! 口径：
//! - 优先级只影响排队顺序，不绕过并发闸门（`running < limit` 不超卖）；
//! - 等待 ≥5min 触发饥饿保护（每 run 审计一次）；
//! - run 超时先 interrupt，5s 未收口 → `timeout` 终态；自动重试按错误分类与退避；
//! - `queue_list` 返回位置/等待原因/优先级分数；10 会话零错误路由。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::Arc;
use std::time::Duration;

use aether_control::{FixedFreeRam, LifecycleConfig, RunScheduleOptions, STARVATION_GUARD_MS};
use aether_core::{ErrorInfo, RunStatus};
use aether_store::AuditQuery;
use m2_support::{
    build_manager, create_session, manual_clock, system_clock, wait_for, wait_run_status,
    CancellableExecutor, ScriptedExecutor, TestCore, UnresponsiveExecutor,
};

fn schedule(priority: i32, timeout_ms: i64, max_attempts: u32) -> RunScheduleOptions {
    RunScheduleOptions {
        priority,
        timeout_ms,
        max_attempts,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn priority_queue_orders_by_score_and_reports_queue_list() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    )
    .with_free_ram(Arc::new(FixedFreeRam::new(3.0)));
    // 闸门上限 = clamp(floor(3/1.5),2,10) = 2（前两条派发，其余排队）。
    assert_eq!(manager.refresh_gate_limit().await, Some(2));

    let first = create_session(&manager, "sched-a").await;
    let second = create_session(&manager, "sched-b").await;
    let low = create_session(&manager, "sched-low").await;
    let high = create_session(&manager, "sched-high").await;

    let ack_a = manager
        .send_with_schedule(
            &first.id,
            "a",
            "01J000000000000000000000A1",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(0, 1_800_000, 1),
        )
        .await
        .unwrap();
    let ack_b = manager
        .send_with_schedule(
            &second.id,
            "b",
            "01J000000000000000000000B1",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(0, 1_800_000, 1),
        )
        .await
        .unwrap();
    let ack_low = manager
        .send_with_schedule(
            &low.id,
            "low",
            "01J000000000000000000000L1",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(50, 1_800_000, 1),
        )
        .await
        .unwrap();
    let ack_high = manager
        .send_with_schedule(
            &high.id,
            "high",
            "01J000000000000000000000H1",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(-50, 1_800_000, 1),
        )
        .await
        .unwrap();

    assert!(!ack_a.queued && ack_a.queue.is_none(), "前两条应直接派发");
    assert!(!ack_b.queued && ack_b.queue.is_none());
    assert!(
        ack_low.queue.is_some(),
        "priority=50 进入全局队列（位置为入队时快照）"
    );
    assert_eq!(
        ack_high.queue.map(|queue| queue.position),
        Some(1),
        "priority=-50 入队后位于队首"
    );
    assert!(
        wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await,
        "前两条应占用 2 个闸门槽位"
    );
    let gate = manager.concurrency_gate().await;
    assert_eq!(gate.limit, 2);
    assert_eq!(gate.active, 2);
    assert_eq!(gate.queued, 2);

    let list = manager.queue_list(None, 100, 0).await;
    assert_eq!(list.total, 2);
    assert_eq!(list.items[0].run_id, ack_high.run_id, "低分数优先");
    assert_eq!(list.items[0].priority, -50);
    assert_eq!(list.items[0].position, 1);
    assert_eq!(list.items[0].wait_reason, "concurrency_gate");
    assert_eq!(list.items[1].run_id, ack_low.run_id);
    assert_eq!(list.items[1].position, 2);

    // 释放 1 个槽位：GateQueue 按分数推进（单测覆盖 pop 顺序；集成层断言队列
    // 快照与最终收口，避免任务调度竞态干扰）。
    executor.release(1);
    assert!(
        wait_for(|| executor.call_count() >= 3, Duration::from_secs(5)).await,
        "释放槽位后应推进排队 run"
    );
    executor.release(3);
    for ack in [&ack_a, &ack_b, &ack_low, &ack_high] {
        assert!(
            wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
            "run 应成功：{}",
            ack.run_id
        );
    }
    let final_list = manager.queue_list(None, 100, 0).await;
    assert_eq!(final_list.total, 0, "全部收口后队列应为空");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_boost_and_starvation_guard_audit_once() {
    let core = TestCore::open().await;
    let clock = manual_clock(1_000_000);
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        LifecycleConfig::default(),
    )
    .with_free_ram(Arc::new(FixedFreeRam::new(3.0)));
    assert_eq!(manager.refresh_gate_limit().await, Some(2));

    let running = create_session(&manager, "starve-running").await;
    let running2 = create_session(&manager, "starve-running-2").await;
    let waiting = create_session(&manager, "starve-waiting").await;
    let _ack_running = manager
        .send(&running.id, "r", "01J000000000000000000000R1")
        .await
        .unwrap();
    let _ack_running2 = manager
        .send(&running2.id, "r2", "01J000000000000000000000R2")
        .await
        .unwrap();
    let ack_waiting = manager
        .send(&waiting.id, "w", "01J000000000000000000000W1")
        .await
        .unwrap();
    assert!(ack_waiting.queue.is_some());

    clock.advance(STARVATION_GUARD_MS);
    let list = manager.queue_list(None, 100, 0).await;
    assert_eq!(list.total, 1);
    assert_eq!(list.items[0].wait_ms, STARVATION_GUARD_MS);
    assert!(
        list.gate.starvation_guard.unwrap_or(0) >= 1,
        "5min 等待应计入 starvation_guard"
    );

    assert_eq!(manager.sweep_starvation_guard_once().await, 1);
    assert_eq!(
        manager.sweep_starvation_guard_once().await,
        0,
        "每 run 只审计一次"
    );
    let audit = core
        .reads()
        .audit_query(AuditQuery {
            action: Some("scheduler.starvation_guard".to_owned()),
            limit: Some(10),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.rows.len(), 1);
    assert_eq!(
        audit.rows[0].session_id.as_deref(),
        Some(waiting.id.as_str())
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_interrupts_then_marks_timeout_and_auto_retries() {
    let core = TestCore::open().await;
    let clock = manual_clock(1_000_000);
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "timeout-auto-retry").await;
    let ack = manager
        .send_with_schedule(
            &session.id,
            "long",
            "01J000000000000000000000T1",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(0, 1_000, 2),
        )
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "run 应派发"
    );

    // 到点：先 interrupt（取消令牌），执行器在 5s 宽限内返回 Cancelled → 仍按 timeout 收口。
    clock.advance(1_000);
    let _ = manager.watchdog_once().await;
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Timeout).await,
        "超时 run 必须收口为 timeout"
    );
    assert!(
        wait_for(
            || manager.pending_retry_count() == 1,
            Duration::from_secs(5)
        )
        .await,
        "timeout + max_attempts=2 应登记自动重试"
    );

    // 退避后执行自动重试（1s ±20% → 2s 足够）。
    clock.advance(2_000);
    assert_eq!(manager.process_pending_retries_once().await, 1);
    let attempts = {
        let old = manager.run(&ack.run_id).await.unwrap().unwrap();
        core.reads()
            .runs_for_input_message(old.input_message_id.as_ref().unwrap())
            .await
            .unwrap()
    };
    assert_eq!(attempts.len(), 2, "旧 attempt 终态保留 + 新 attempt");
    assert!(
        attempts.iter().any(|row| row.retry_count == 1),
        "新 attempt retry_count=1：{attempts:?}"
    );
    let new_run = attempts
        .iter()
        .find(|row| row.run_id != ack.run_id)
        .expect("新 attempt")
        .run_id
        .clone();
    executor.release(1);
    assert!(
        wait_run_status(&manager, &new_run, RunStatus::Succeeded).await,
        "自动重试的新 attempt 应成功"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresponsive_timeout_is_forced_after_grace_and_audited() {
    let core = TestCore::open().await;
    let clock = manual_clock(1_000_000);
    let executor = UnresponsiveExecutor::new();
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "timeout-forced").await;
    let ack = manager
        .send_with_schedule(
            &session.id,
            "hang",
            "01J000000000000000000000T2",
            None,
            Vec::new(),
            Vec::new(),
            None,
            schedule(0, 1_000, 1),
        )
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);

    clock.advance(1_000);
    let _ = manager.watchdog_once().await; // 请求中断
    assert!(
        manager.run(&ack.run_id).await.unwrap().unwrap().status == RunStatus::Running,
        "宽限期内不提前标 timeout"
    );
    clock.advance(5_001);
    let _ = manager.watchdog_once().await; // 宽限超时 → 强制 timeout
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Timeout).await,
        "不响应执行器必须在宽限后强制 timeout"
    );
    let audit = core
        .reads()
        .audit_query(AuditQuery {
            action: Some("run.timeout".to_owned()),
            limit: Some(10),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.rows.len(), 1, "超时审计必须落库");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_retry_rejects_non_retryable_classification() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "retry-classification").await;
    executor.set_outcome(aether_control::ExecutorOutcome::Failed {
        error: ErrorInfo {
            code: "provider_auth_failed".to_owned(),
            message: "401".to_owned(),
            recoverable: false,
        },
    });
    let ack = manager
        .send(&session.id, "auth", "01J000000000000000000000C1")
        .await
        .unwrap();
    assert!(wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await);
    executor.release(1);
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Failed).await,
        "run 应失败"
    );
    let error = manager.retry_run(&ack.run_id).await.unwrap_err();
    assert_eq!(error.code(), "invalid_value");
    assert!(matches!(
        error,
        aether_control::LifecycleError::RetryNotAllowed { .. }
    ));

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent_sessions_complete_without_oversell() {
    const SESSIONS: usize = 10;
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    )
    .with_free_ram(Arc::new(FixedFreeRam::new(16.0)));
    assert_eq!(manager.refresh_gate_limit().await, Some(10));

    let mut acks = Vec::new();
    for index in 0..SESSIONS {
        let session = create_session(&manager, &format!("ten-{index}")).await;
        let ack = manager
            .send(
                &session.id,
                "go",
                &format!("01J0000000000000000000{index:04}"),
            )
            .await
            .unwrap();
        acks.push(ack);
    }
    let gate = manager.concurrency_gate().await;
    assert_eq!(gate.limit, 10);
    assert_eq!(gate.active, SESSIONS);
    assert_eq!(gate.queued, 0, "10 并发不超卖/不排队");

    executor.release(SESSIONS);
    for ack in &acks {
        assert!(
            wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
            "run 应成功：{}",
            ack.run_id
        );
    }
    let after = manager.concurrency_gate().await;
    assert_eq!(after.active, 0);
    assert_eq!(after.queued, 0);
    assert_eq!(executor.call_count(), SESSIONS);
    assert_eq!(manager.queue_list(None, 100, 0).await.total, 0);

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

#[test]
fn schedule_options_validate_adr_ranges() {
    assert!(schedule(0, 1_800_000, 1).validate().is_ok());
    assert!(schedule(101, 1_800_000, 1).validate().is_err());
    assert!(schedule(0, 999, 1).validate().is_err());
    assert!(schedule(0, 1_800_000, 6).validate().is_err());
}
