//! M1-05 DoD②/⑥：故障注入与持久化降级状态机（设计 D4；ADR-003/ADR-004 边界）。
//!
//! 覆盖：
//! - DoD② journal 写失败重试 3 次 → `persist_degraded` + 只读；拒绝新写入/新 run；
//!   未落盘事件不广播（调用序断言）；在途 run 转 `cancelled`；
//! - 降级通知：`error(recoverable=false)` 落盘成功才广播；落盘失败仅经 `health`；
//! - DoD⑥ 进入/退出断言：写失败/空间护栏/完整性失败；退出 = 重启 + 启动自检通过；
//!   写队列临时高水位（≤L2）**不得**进入本状态；降级经 `health` 返回 `storage_state`。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aether_control::{
    DegradeTrigger, EventPipeline, JournalError, JournalMetrics, PipelineConfig, PressureLevel,
    StartupSelfCheckReport, StorageState,
};
use aether_core::{EventPayload, EventType, SessionId};
use tokio::runtime::Handle;

use common::{
    delta_event, log_envelope, log_event, run_started_event, start_pipeline, start_pipeline_with,
    Behavior, FakeJournal, MESSAGE_1, RUN_1, SESSION_A,
};

/// DoD②：写失败重试 3 次均失败 → 降级 + 只读；未落盘不广播；在途 run cancelled。
#[tokio::test]
async fn dod2_write_failure_retries_three_times_then_persist_degraded() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let mut subscriber = pipeline.subscribe();
    let mut interrupts = pipeline.subscribe_run_interrupts();

    // 建立在途 run（正常落盘）。
    let started = run_started_event("01J0000000000000000000S01", SESSION_A, RUN_1);
    pipeline.submit(started).await.unwrap();
    assert_eq!(pipeline.health().in_flight_runs, 1);
    let started_event = subscriber.recv().await.unwrap();
    assert_eq!(started_event.event_type(), EventType::RunStarted);

    // 故障注入：后续 3 次写事务连续失败（降级 error 事件落盘成功 → 广播）。
    journal.script_repeat(Behavior::fail("database or disk is full"), 3);
    let failing = log_event("01J0000000000000000000F01", SESSION_A);
    let failing_id = failing["id"].as_str().unwrap().to_owned();
    let error = pipeline.submit(failing).await.unwrap_err();
    assert_eq!(error.code(), "persist_degraded");

    // 调用序断言：失败事件被尝试 3 次，且从未广播。
    let calls = journal.calls();
    assert_eq!(
        calls.len(),
        5,
        "run.started 1 次 + 失败事件 3 次 + 降级 error 1 次"
    );
    let attempts = calls
        .iter()
        .filter(|batch| batch.iter().any(|event| event.id.as_str() == failing_id))
        .count();
    assert_eq!(attempts, 3, "写事务必须重试 3 次");
    let timeline = journal.timeline();
    assert_eq!(
        timeline
            .iter()
            .filter(|entry| entry.as_str() == format!("append:{failing_id}"))
            .count(),
        3
    );
    let mut received = Vec::new();
    while let Ok(event) = subscriber.try_recv() {
        received.push(event);
    }
    assert!(
        received.iter().all(|event| event.id.as_str() != failing_id),
        "未落盘事件不得广播: {received:?}"
    );
    // 降级 error 事件落盘成功 → 广播 `error(recoverable=false)`。
    let notice = received
        .iter()
        .find(|event| event.event_type() == EventType::Error)
        .expect("降级 error 事件必须广播");
    match &notice.payload {
        EventPayload::Error(info) => {
            assert_eq!(info.code, "persist_degraded");
            assert!(!info.recoverable, "降级通知必须 recoverable=false");
        }
        other => panic!("类型不符: {other:?}"),
    }

    // 在途 run 转 cancelled：控制通道通知 + 台账清零。
    let interrupt = interrupts.try_recv().expect("在途 run 必须收到中断通知");
    assert_eq!(interrupt.run_id.as_str(), RUN_1);
    assert_eq!(interrupt.reason, "persist_degraded");

    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::PersistDegraded);
    assert_eq!(health.storage_state_code(), "persist_degraded");
    assert_eq!(
        health.degrade_trigger.as_ref().map(DegradeTrigger::code),
        Some("write_failure")
    );
    assert!(health.degraded_since_ms.is_some());
    assert_eq!(health.persist_retries, 3, "失败尝试累计 3 次");
    assert_eq!(health.dropped_events, 1, "未落盘事件必须计数");
    assert_eq!(health.cancelled_runs, 1);
    assert_eq!(health.in_flight_runs, 0);

    // 拒绝新写入/新 run：不再触发任何 journal 调用。
    let before = journal.call_count();
    let error = pipeline
        .submit(log_event("01J0000000000000000000F02", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    assert_eq!(journal.call_count(), before, "降级期不得再尝试写入");
    assert_eq!(
        pipeline.admission().unwrap_err().code(),
        "persist_degraded",
        "降级期拒绝新 run 准入"
    );
}

/// ADR-007 决策 1：`health` 返回 `normal` 与 `persist_degraded` 两态
/// （命令层 `health` 接线与 E2E 归 M2-07；本用例锁定管线侧契约）。
#[tokio::test]
async fn adr007_health_reports_both_storage_states() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let normal = pipeline.health();
    assert_eq!(normal.storage_state_code(), "normal");
    assert!(normal.degrade_trigger.is_none());

    pipeline
        .signal_degraded(DegradeTrigger::SpaceGuard { free_bytes: 1 })
        .await
        .unwrap();
    let degraded = pipeline.health();
    assert_eq!(degraded.storage_state_code(), "persist_degraded");
    assert_eq!(
        degraded.degrade_trigger.as_ref().map(DegradeTrigger::code),
        Some("space_guard")
    );
    assert!(degraded.degraded_since_ms.is_some());
    // 降级期健康查询仍可用（读路径不写库）。
    assert_eq!(degraded.journal_queue_depth, 0);
}

/// ADR-007 决策 2：第 1、2 次失败、第 3 次成功 → **不降级**，attempt 日志止于 2/3。
#[tokio::test]
async fn dod2_two_failures_then_success_does_not_degrade() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    journal.script([
        Behavior::fail("第一次失败"),
        Behavior::fail("第二次失败"),
        Behavior::Ok,
    ]);
    let outcome = pipeline
        .submit(log_event("01J0000000000000000000A01", SESSION_A))
        .await
        .unwrap();
    assert!(
        outcome.is_persisted(),
        "第 3 次尝试成功必须落盘: {outcome:?}"
    );
    assert_eq!(outcome.seq(), Some(1));
    assert_eq!(journal.call_count(), 3, "共 3 次尝试（含首次）");

    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::Normal, "不得降级");
    assert_eq!(health.persist_retries, 2, "失败尝试记录 2 次");
    assert_eq!(journal.persisted().len(), 1, "事件正常落盘一次");
    assert_eq!(pipeline.health().dropped_events, 0, "无未落盘事件");
}

/// DoD②（另一分支）：降级 error 事件也落盘失败 → 不广播，仅经 health 呈现。
#[tokio::test]
async fn dod2_degraded_notice_not_broadcast_when_error_event_cannot_persist() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    let mut subscriber = pipeline.subscribe();

    journal.script_repeat(Behavior::fail("IO error"), 4);
    let error = pipeline
        .submit(log_event("01J0000000000000000000F03", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    assert_eq!(journal.call_count(), 4, "3 次事件重试 + 1 次降级通知尝试");

    // 不广播任何事件（无「仅内存广播」路径）。
    assert!(
        subscriber.try_recv().is_err(),
        "降级本身即写失败场景：不广播"
    );
    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::PersistDegraded);
    assert_eq!(health.persisted_events, 0);
    assert_eq!(health.broadcast_events, 0);
    assert_eq!(health.dropped_events, 2, "失败事件 + 落盘失败的降级通知");
}

/// DoD②：重复 seq（DB UNIQUE 兜底）视为管线 bug——计入诊断并按持久化失败路径处理。
#[tokio::test]
async fn dod2_duplicate_seq_is_counted_as_pipeline_bug_and_degrades() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    journal.script_repeat(
        Behavior::DuplicateSeq {
            message: "UNIQUE constraint failed: events.session_id, events.seq".to_owned(),
        },
        3,
    );
    let error = pipeline
        .submit(log_event("01J0000000000000000000F04", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    let health = pipeline.health();
    assert_eq!(health.duplicate_seq_bugs, 3, "每次兜底命中都必须计入诊断");
    assert_eq!(
        health.degrade_trigger.as_ref().map(DegradeTrigger::code),
        Some("write_failure")
    );
    match health.degrade_trigger {
        Some(DegradeTrigger::WriteFailure { last_error, .. }) => {
            assert!(
                last_error.contains("UNIQUE"),
                "触发源需保留原始错误: {last_error}"
            );
        }
        other => panic!("触发源不符: {other:?}"),
    }
}

/// DoD②：降级丢弃未落盘 delta（不广播）并取消在途 run。
///
/// 本例中失败事件提交前先冲刷同会话 pending delta（保序），故失败落在 delta 上：
/// delta 不落库、不广播，随后进入降级并取消在途 run。
#[tokio::test]
async fn dod2_degrade_discards_unpersisted_delta_and_cancels_runs() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    );
    let mut interrupts = pipeline.subscribe_run_interrupts();
    pipeline
        .submit(run_started_event(
            "01J0000000000000000000S02",
            SESSION_A,
            RUN_1,
        ))
        .await
        .unwrap();
    pipeline
        .submit(delta_event(
            "01J0000000000000000000S03",
            SESSION_A,
            MESSAGE_1,
            "未落盘",
        ))
        .await
        .unwrap();

    journal.script_repeat(Behavior::fail("disk full"), 3);
    let error = pipeline
        .submit(log_event("01J0000000000000000000S04", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");

    let stored = journal.persisted();
    let types: Vec<EventType> = stored.iter().map(|event| event.event_type()).collect();
    assert_eq!(
        types,
        vec![EventType::RunStarted, EventType::Error],
        "未落盘 delta 必须丢弃（无 message.delta 行）"
    );
    let health = pipeline.health();
    assert_eq!(health.delta_persisted_events, 0);
    assert_eq!(health.dropped_events, 1, "失败事件（delta 冲刷）计数");
    assert_eq!(health.persist_retries, 3);
    assert_eq!(health.cancelled_runs, 1);
    let interrupt = interrupts.try_recv().expect("在途 run 必须收到中断通知");
    assert_eq!(interrupt.session_id.as_str(), SESSION_A);
}

/// DoD②：降级由其他会话触发时，未落盘 delta 缓冲被丢弃并计数（D4 降级期语义 2/3）。
#[tokio::test]
async fn dod2_degrade_clears_other_session_delta_buffers() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    );
    pipeline
        .submit(delta_event(
            "01J0000000000000000000T01",
            common::SESSION_B,
            MESSAGE_1,
            "ab",
        ))
        .await
        .unwrap();
    pipeline
        .submit(delta_event(
            "01J0000000000000000000T02",
            common::SESSION_B,
            MESSAGE_1,
            "cd",
        ))
        .await
        .unwrap();

    journal.script_repeat(Behavior::fail("disk full"), 3);
    let error = pipeline
        .submit(log_event("01J0000000000000000000T03", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");

    let health = pipeline.health();
    assert_eq!(
        health.delta_buffers_discarded, 2,
        "跨会话未落盘 delta 必须丢弃并计数"
    );
    assert_eq!(health.dropped_events, 3, "失败事件 1 + 丢弃 delta 2");
    let stored = journal.persisted();
    let types: Vec<EventType> = stored.iter().map(|event| event.event_type()).collect();
    assert_eq!(types, vec![EventType::Error], "丢弃的 delta 不落库");
}

/// DoD⑥：写队列临时高水位（≤L2）**不得**进入 `persist_degraded`（ADR-004 决策 1）。
#[tokio::test]
async fn dod6_temporary_high_water_stays_normal() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);

    // 高水位重试后回落 → 正常落盘，状态不变。
    journal.script([
        Behavior::Backpressure {
            depth: 4_200,
            threshold: 4_096,
        },
        Behavior::Backpressure {
            depth: 4_100,
            threshold: 4_096,
        },
        Behavior::Ok,
    ]);
    let outcome = pipeline
        .submit(log_event("01J0000000000000000000W01", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(1));
    assert_eq!(pipeline.health().storage_state, StorageState::Normal);
    assert_eq!(pipeline.health().backpressure_rejections, 0);
    assert_eq!(
        pipeline.health().persist_retries,
        0,
        "背压等待不计入持久化重试"
    );

    // 持续高水位 → 拒绝本次准入（storage_backpressure），仍不降级。
    // 持续高水位 → 拒绝本次准入（storage_backpressure），仍不降级。
    journal.script_repeat(
        Behavior::Backpressure {
            depth: 4_500,
            threshold: 4_096,
        },
        3,
    );
    let error = pipeline
        .submit(log_event("01J0000000000000000000W02", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "storage_backpressure");
    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::Normal);
    assert_eq!(health.backpressure_rejections, 1);
    assert_eq!(health.persist_retries, 0);

    // 准入接口的高水位注入（D8 L2 接口）同样不改变存储状态。
    journal.set_admission(Some(JournalError::Backpressure {
        depth: 4_800,
        threshold: 4_096,
    }));
    assert_eq!(
        pipeline.admission().unwrap_err().code(),
        "storage_backpressure"
    );
    assert_eq!(pipeline.health().storage_state, StorageState::Normal);

    // 队列深度进 health（诊断证据），仍为临时高水位语义。
    journal.set_metrics(JournalMetrics {
        queue_depth: 4_800,
        pressure_level: Some(PressureLevel::L2),
    });
    let health = pipeline.health();
    assert_eq!(health.journal_queue_depth, 4_800);
    assert_eq!(health.journal_pressure_level, Some(PressureLevel::L2));
    assert_eq!(health.storage_state, StorageState::Normal);
    assert!(
        health.dropped_events >= 1,
        "被拒绝的事件计为未落盘丢弃（但不降级）"
    );
}

/// DoD⑥：空间护栏/完整性失败信号进入降级；首次触发源胜出；无热恢复。
#[tokio::test]
async fn dod6_runtime_signals_and_no_hot_recovery() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline(&journal);
    assert_eq!(pipeline.health().storage_state, StorageState::Normal);

    let transitioned = pipeline
        .signal_degraded(DegradeTrigger::SpaceGuard { free_bytes: 1024 })
        .await
        .unwrap();
    assert!(transitioned);
    assert!(!pipeline
        .signal_degraded(DegradeTrigger::WriteFailure {
            attempts: 3,
            last_error: "later".to_owned(),
        })
        .await
        .unwrap());
    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::PersistDegraded);
    assert_eq!(
        health.degrade_trigger.as_ref().map(DegradeTrigger::code),
        Some("space_guard"),
        "首次触发源胜出"
    );

    // 降级不可热恢复：状态机无运行期恢复 API；重复信号不改变已记录触发源。
    assert_eq!(
        pipeline.health().storage_state,
        StorageState::PersistDegraded
    );
}

/// DoD⑥：启动自检失败 → 以只读降级启动；读路径保持可用。
#[tokio::test]
async fn dod6_startup_check_failure_starts_read_only() {
    let journal = FakeJournal::new();
    journal.seed([
        log_envelope("01J00000000000000000000S1", SESSION_A, 1),
        log_envelope("01J00000000000000000000S2", SESSION_A, 2),
    ]);
    let pipeline = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(journal.clone()),
        Arc::new(journal.source()),
        &StartupSelfCheckReport::failing_integrity("page 3 校验失败"),
        &Handle::current(),
    )
    .unwrap();

    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::PersistDegraded);
    assert_eq!(
        health.degrade_trigger.as_ref().map(DegradeTrigger::code),
        Some("integrity_failure")
    );
    let error = pipeline
        .submit(log_event("01J00000000000000000000S3", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    assert_eq!(journal.call_count(), 0, "只读启动不得尝试写入");

    // 读查询（补读）在降级期保持可用（D4 降级期语义 1）。
    let session = SessionId::new(SESSION_A).unwrap();
    let frame = pipeline.readback(&session, 0).await.unwrap();
    assert_eq!(frame.events.len(), 2);
    assert!(frame.complete);
}

/// DoD⑥：退出断言（重启 + 自检通过）——新实例恢复 normal，旧实例保持只读。
#[tokio::test]
async fn dod6_exit_requires_restart_with_passing_check() {
    let journal = FakeJournal::new();
    let degraded = start_pipeline(&journal);
    degraded
        .signal_degraded(DegradeTrigger::SpaceGuard { free_bytes: 1 })
        .await
        .unwrap();
    assert_eq!(
        degraded.health().storage_state,
        StorageState::PersistDegraded
    );

    // 模拟「修复外部条件 + 重启核心 + 启动自检通过」：同库上创建新管线。
    let resumed = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(journal.clone()),
        Arc::new(journal.source()),
        &StartupSelfCheckReport::passing(8 * 1024 * 1024 * 1024),
        &Handle::current(),
    )
    .unwrap();
    let outcome = resumed
        .submit(log_event("01J0000000000000000000X01", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(1), "重启后 seq 从库中 max+1 恢复");
    assert_eq!(resumed.health().storage_state, StorageState::Normal);

    // P0 无热恢复：旧实例仍是只读（状态只随进程重启重置）。
    assert_eq!(
        degraded.health().storage_state,
        StorageState::PersistDegraded
    );
    assert!(degraded
        .submit(log_event("01J0000000000000000000X02", SESSION_A))
        .await
        .is_err());
}
