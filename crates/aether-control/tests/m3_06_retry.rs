//! M3-06 集成测试：`run_retry` 一键重放与重启状态重建（设计 D2/D4/D5/D8、ADR-004/ADR-005）。
//!
//! 覆盖：
//! - 准入：仅终态（`failed`/`timeout`/`cancelled`）可重试；不存在 → `run_not_found`；
//!   非终态 → `run_not_retryable`；终态会话 → `session_closed`；降级 → `persist_degraded`；
//! - 重放：复用原输入消息（不新增用户消息行）、新 run 重新编号、旧 run 保留审计；
//!   派发经 run 串行（忙时入等待队列）；
//! - 重启状态重建：`queued`/`running` run 收口为 `failed`（`run_interrupted`，可重试）
//!   + 非空闲会话回 `idle`；事件经管线先日志后广播（`run.failed`/`session.status_changed`）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::atomic::{AtomicUsize, Ordering};

use aether_control::{DegradeTrigger, ExecutorOutcome};
use aether_core::{ErrorInfo, RunStatus, SessionStatus};
use m2_support::{
    build_manager, create_session, message_count, reopen_core, system_clock, wait_run_status,
    wait_session_status, ScriptedExecutor, TestCore,
};

fn client_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn failed_outcome(code: &str) -> ExecutorOutcome {
    ExecutorOutcome::Failed {
        error: ErrorInfo {
            code: code.to_owned(),
            message: "脚本失败".to_owned(),
            recoverable: true,
        },
    }
}

/// DoD2：仅终态可重试；重放复用输入消息、新 run 编号、旧 run 保留审计。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_requires_terminal_run_and_preserves_old_run() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "retry").await;

    // 1) 成功的 run 不可重试（仅终态中的 failed/timeout/cancelled 可重试）。
    let first = manager
        .send(&session.id, "first", &client_id("first"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &first.run_id, RunStatus::Succeeded).await,
        "首 run 应成功"
    );
    let error = manager.retry_run(&first.run_id).await.unwrap_err();
    assert_eq!(error.code(), "run_not_retryable");
    assert!(error.to_string().contains("succeeded"));

    // 2) 失败的 run 可重试：新 run 复用同一输入消息，旧 run 行保留。
    executor.set_outcome(failed_outcome("script_failed"));
    let second = manager
        .send(&session.id, "second", &client_id("second"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Failed).await,
        "第二个 run 应失败"
    );
    let messages_before = message_count(&core.reads(), &session.id).await;
    executor.set_outcome(ExecutorOutcome::Completed {
        assistant_text: Some("重放完成".to_owned()),
        usage: None,
    });
    let retry = manager.retry_run(&second.run_id).await.unwrap();
    assert_eq!(retry.session_id, session.id);
    assert_ne!(retry.run_id, second.run_id, "重放必须产生新 run");
    assert_eq!(
        retry.input_message_id, second.message_id,
        "重放复用原输入消息（不新增用户消息行）"
    );
    assert!(!retry.queued);
    assert!(
        wait_run_status(&manager, &retry.run_id, RunStatus::Succeeded).await,
        "重放 run 应成功"
    );

    // 旧 run 保留审计（行仍在、错误码不变）；消息数只增助手终稿 1 条。
    let old = manager
        .run(&second.run_id)
        .await
        .unwrap()
        .expect("旧 run 行");
    assert_eq!(old.status, RunStatus::Failed);
    assert_eq!(old.error.as_deref(), Some("script_failed"));
    let retried = manager
        .run(&retry.run_id)
        .await
        .unwrap()
        .expect("新 run 行");
    assert_eq!(retried.input_message_id, Some(second.message_id.clone()));
    assert_eq!(
        message_count(&core.reads(), &session.id).await,
        messages_before + 1,
        "重放不新增用户消息（仅助手终稿 +1）"
    );

    // 3) 未知 run → run_not_found。
    let missing = aether_core::RunId::new("01J00000000000000000000MISS").unwrap();
    assert_eq!(
        manager.retry_run(&missing).await.unwrap_err().code(),
        "run_not_found"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2：运行中的 run 不可重试；中断后的 run 可重试且会话回 idle 后立即派发。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_run_rejected_then_cancelled_run_retries() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "retry-cancel").await;

    let ack = manager
        .send(&session.id, "long", &client_id("long"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Running).await,
        "run 应在执行中"
    );
    assert_eq!(
        manager.retry_run(&ack.run_id).await.unwrap_err().code(),
        "run_not_retryable",
        "运行中 run 拒绝重试"
    );

    manager.interrupt(&session.id).await.unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Cancelled).await,
        "中断后 run 应为 cancelled"
    );
    let retry = manager.retry_run(&ack.run_id).await.unwrap();
    // 旧任务仍阻塞在许可上（脚本执行器忽略取消），放行 2 个许可：旧任务与重放任务。
    executor.release(2);
    assert!(
        wait_run_status(&manager, &retry.run_id, RunStatus::Succeeded).await,
        "中断 run 的重放应成功"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2：存储降级（`persist_degraded`）期间拒绝重放（D4：拒绝新 run）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_is_rejected_while_persist_degraded() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "retry-degraded").await;
    executor.set_outcome(failed_outcome("script_failed"));
    let ack = manager
        .send(&session.id, "boom", &client_id("boom"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Failed).await,
        "run 应失败（终态可重试）"
    );

    assert!(
        core.pipeline
            .signal_degraded(DegradeTrigger::WriteFailure {
                attempts: 3,
                last_error: "注入".to_owned(),
            })
            .await
            .unwrap(),
        "注入降级应进入 persist_degraded"
    );
    let error = manager.retry_run(&ack.run_id).await.unwrap_err();
    assert_eq!(error.code(), "persist_degraded");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD1：重启状态重建——未收口 run 收口为 `failed`（可重试）+ 会话回 idle + 事件落库。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_reconciles_unfinished_runs_and_sessions() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "reconcile").await;
    let running = manager
        .send(&session.id, "running", &client_id("running"))
        .await
        .unwrap();
    let queued = manager
        .send(&session.id, "queued", &client_id("queued"))
        .await
        .unwrap();
    assert!(queued.queued, "第二条应进入等待队列");
    assert!(
        wait_run_status(&manager, &running.run_id, RunStatus::Running).await,
        "第一条应在执行中"
    );

    // 模拟核心崩溃：关闭存储/管线（不执行 run 终态收口），在既有数据目录上重开。
    let (temp, db_path) = core.shutdown().await;
    let core = reopen_core(temp, db_path).await;
    let executor2 = ScriptedExecutor::new(4);
    let manager2 = build_manager(
        &core,
        system_clock(),
        executor2.clone(),
        aether_control::LifecycleConfig::default(),
    );

    let report = manager2.reconcile_interrupted_runs().await.unwrap();
    assert_eq!(
        report.runs_failed.len(),
        2,
        "queued + running 均应收口：{:?}",
        report.runs_failed
    );
    assert_eq!(report.sessions_reset, vec![session.id.clone()]);

    for run_id in [&running.run_id, &queued.run_id] {
        let run = manager2.run(run_id).await.unwrap().expect("run 行");
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "未收口 run 必须收口为 failed"
        );
        assert_eq!(
            run.error.as_deref(),
            Some(aether_control::RUN_INTERRUPTED_CODE)
        );
    }
    assert!(
        wait_session_status(&manager2, &session.id, SessionStatus::Idle).await,
        "在途会话必须回 idle"
    );

    // 事件经管线落库（先日志后广播）：run.failed ×2 + session.status_changed。
    let frame = core.pipeline.readback(&session.id, 0).await.unwrap();
    let failed_events = frame
        .events
        .iter()
        .filter(|event| {
            event.event_type() == aether_core::EventType::RunFailed
                && event.run_id.as_ref() == Some(&running.run_id)
        })
        .count();
    assert_eq!(failed_events, 1, "running run 的 run.failed 事件必须落库");
    let queued_failed = frame
        .events
        .iter()
        .filter(|event| {
            event.event_type() == aether_core::EventType::RunFailed
                && event.run_id.as_ref() == Some(&queued.run_id)
        })
        .count();
    assert_eq!(queued_failed, 1, "queued run 的 run.failed 事件必须落库");
    let status_events = frame
        .events
        .iter()
        .filter(|event| event.event_type() == aether_core::EventType::SessionStatusChanged)
        .count();
    assert!(status_events >= 1, "会话回 idle 事件必须落库");

    // 收口后的 run 可一键重放（终态准入）。
    let retry = manager2.retry_run(&running.run_id).await.unwrap();
    assert!(
        wait_run_status(&manager2, &retry.run_id, RunStatus::Succeeded).await,
        "收口 run 的重放应成功"
    );

    // 幂等：重放收口（run 终态 + 会话回 idle）后再次收口无待处理项。
    assert!(
        wait_session_status(&manager2, &session.id, SessionStatus::Idle).await,
        "重放后会话应回 idle"
    );
    let second = manager2.reconcile_interrupted_runs().await.unwrap();
    assert!(second.runs_failed.is_empty());
    assert!(second.sessions_reset.is_empty());

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
