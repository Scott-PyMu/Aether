//! M1-05 真实存储集成测试：管线 × D3 单写队列 + WAL 读连接池（设计 D3/D4）。
//!
//! 覆盖：
//! - 真实 SQLite：seq 单调唯一落库；sequencer 重启从库中 `max(seq)+1` 恢复；
//! - 真实 SQLite：delta 合并后的行与 `message.completed` 终稿一致（补读回读验证）；
//! - 真实故障注入：写队列关闭 → 写失败重试 3 次 → `persist_degraded`；
//!   读查询保持可用、无部分写入；在途 run 转 cancelled；
//! - DB 兜底语义：`events.id` 主键冲突 → 幂等丢弃；`UNIQUE(session_id, seq)` → 管线 bug 分类；
//! - 退出路径：启动自检失败 → 只读；修复 + 重启（新实例 + 自检通过）→ 恢复写入、seq 续接。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aether_control::{
    EventPipeline, JournalError, JournalWriter, PipelineConfig, StartupSelfCheckReport,
    StorageState, StoreEventSource, StoreJournal,
};
use aether_core::{EventPayload, EventType, SessionId};
use aether_store::{StoreRuntime, WriteQueueConfig};
use tempfile::TempDir;
use tokio::runtime::Handle;

use common::{
    completed_event, delta_event, log_envelope, log_event, run_started_event, MESSAGE_1, RUN_1,
    SESSION_A, SESSION_B,
};

/// 打开真实存储运行时（批量参数收紧，使每次提交立即成为独立事务）。
fn open_runtime(dir: &TempDir) -> StoreRuntime {
    let config = WriteQueueConfig {
        max_batch_entries: 1,
        flush_interval: Duration::from_millis(1),
        ..WriteQueueConfig::default()
    };
    StoreRuntime::open(dir.path().join("aether.db"), config, &Handle::current()).unwrap()
}

/// 用真实 journal/source 启动管线（`runtime` 必须保活以持有写任务）。
fn start_real_pipeline(dir: &TempDir, config: PipelineConfig) -> (StoreRuntime, EventPipeline) {
    let runtime = open_runtime(dir);
    let journal = Arc::new(StoreJournal::new(runtime.queue().clone()));
    let source = Arc::new(StoreEventSource::new(runtime.reads().clone()));
    let pipeline = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..config
        },
        journal,
        source,
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &Handle::current(),
    )
    .unwrap();
    (runtime, pipeline)
}

/// 真实 SQLite：seq 单调唯一落库；sequencer 重启后从库中 max+1 恢复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_store_seq_monotonic_and_restart_resumes_from_db() {
    let dir = TempDir::new().unwrap();
    let (runtime, pipeline) = start_real_pipeline(&dir, PipelineConfig::default());

    for index in 1..=30u64 {
        let outcome = pipeline
            .submit(log_event(&format!("01J{index:023}"), SESSION_A))
            .await
            .unwrap();
        assert_eq!(outcome.seq(), Some(index));
    }
    for index in 1..=20u64 {
        let outcome = pipeline
            .submit(log_event(&format!("01J{:023}", 100_000 + index), SESSION_B))
            .await
            .unwrap();
        assert_eq!(outcome.seq(), Some(index), "每会话 seq 独立从 1 起");
    }

    // 直接查库断言（独立于管线）。
    let session_a = SessionId::new(SESSION_A).unwrap();
    assert_eq!(runtime.reads().event_count(&session_a).await.unwrap(), 30);
    assert_eq!(runtime.reads().max_seq(&session_a).await.unwrap(), Some(30));

    // sequencer 重启恢复：seq 从库中 max+1 = 31 继续。
    pipeline.restart_session(session_a.clone()).await.unwrap();
    let outcome = pipeline
        .submit(log_event("01J00000000000000000000Z31", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(31));
    assert_eq!(runtime.reads().max_seq(&session_a).await.unwrap(), Some(31));

    // 库内 seq 严格递增且唯一（补读回读验证）。
    let frame = pipeline.readback(&session_a, 0).await.unwrap();
    let seqs: Vec<u64> = frame.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (1..=31).collect::<Vec<u64>>());
    assert_eq!(pipeline.health().sequencer_restarts, 1);
}

/// 真实 SQLite：delta 合并落库（一条合并行）+ `message.completed` 终稿不受影响。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_store_delta_merge_and_completed_final() {
    let dir = TempDir::new().unwrap();
    let (_runtime, pipeline) = start_real_pipeline(
        &dir,
        PipelineConfig {
            // 窗口设为 5s：本用例由 completed（非 delta 事件）触发保序冲刷，避免时序抖动。
            delta_flush_interval: Duration::from_secs(5),
            ..PipelineConfig::default()
        },
    );
    let mut expected_text = String::new();
    for index in 1..=10u64 {
        let chunk = format!("chunk-{index:02};");
        expected_text.push_str(&chunk);
        let outcome = pipeline
            .submit(delta_event(
                &format!("01J{:023}", 200_000 + index),
                SESSION_A,
                MESSAGE_1,
                &chunk,
            ))
            .await
            .unwrap();
        assert_eq!(outcome, aether_control::SubmitOutcome::Buffered);
    }
    pipeline
        .submit(completed_event(
            "01J000000000000000000200011",
            SESSION_A,
            MESSAGE_1,
            &expected_text,
        ))
        .await
        .unwrap();

    let session_a = SessionId::new(SESSION_A).unwrap();
    let frame = pipeline.readback(&session_a, 0).await.unwrap();
    assert_eq!(
        frame.events.len(),
        2,
        "10 条 delta 必须合并为 1 条 + 1 条终稿"
    );
    match &frame.events[0].payload {
        EventPayload::MessageDelta(delta) => assert_eq!(delta.text, expected_text),
        other => panic!("第 1 条应为合并 delta: {other:?}"),
    }
    assert_eq!(frame.events[0].seq, 1);
    match &frame.events[1].payload {
        EventPayload::MessageCompleted(completed) => {
            assert_eq!(completed.message.content, expected_text, "终稿必须原样保存");
        }
        other => panic!("第 2 条应为 completed: {other:?}"),
    }

    // 8KB 阈值：立即落库（不等窗口）。
    let big = "y".repeat(8 * 1024);
    pipeline
        .submit(delta_event(
            "01J000000000000000000200012",
            SESSION_A,
            MESSAGE_1,
            &big,
        ))
        .await
        .unwrap();
    let frame = pipeline.readback(&session_a, 0).await.unwrap();
    assert_eq!(frame.events.len(), 3, "阈值触发必须立即落库");
}

/// 真实故障注入：写队列关闭（写任务退出）→ 重试 3 次 → `persist_degraded`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_store_write_failure_degrades_while_reads_stay_available() {
    let dir = TempDir::new().unwrap();
    let (runtime, pipeline) = start_real_pipeline(&dir, PipelineConfig::default());
    let mut interrupts = pipeline.subscribe_run_interrupts();

    pipeline
        .submit(run_started_event(
            "01J00000000000000000030R01",
            SESSION_A,
            RUN_1,
        ))
        .await
        .unwrap();
    pipeline
        .submit(log_event("01J00000000000000000030002", SESSION_A))
        .await
        .unwrap();

    // 故障注入：关闭写队列（写任务 drain 后退出）——真实的持久化写失败。
    runtime.shutdown().await.unwrap();

    let error = pipeline
        .submit(log_event("01J00000000000000000030003", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "persist_degraded");
    let health = pipeline.health();
    assert_eq!(health.storage_state, StorageState::PersistDegraded);
    assert_eq!(health.persist_retries, 3, "重试 3 次均失败");
    assert_eq!(
        health.dropped_events, 2,
        "失败事件 1 + 降级通知落盘失败 1（未落盘不广播仅计数）"
    );
    assert_eq!(health.cancelled_runs, 1, "在途 run 转 cancelled");
    let interrupt = interrupts.try_recv().expect("在途 run 必须收到中断通知");
    assert_eq!(interrupt.run_id.as_str(), RUN_1);
    assert_eq!(interrupt.reason, "persist_degraded");

    // 读查询在降级期保持可用（D4 降级期语义 1）；无部分写入。
    let session_a = SessionId::new(SESSION_A).unwrap();
    let frame = pipeline.readback(&session_a, 0).await.unwrap();
    assert_eq!(frame.events.len(), 2, "失败事件不得部分写入");
    let types: Vec<EventType> = frame
        .events
        .iter()
        .map(|event| event.event_type())
        .collect();
    assert_eq!(types, vec![EventType::RunStarted, EventType::Log]);

    // 降级期拒绝新 run（准入）与再次写入。
    assert_eq!(pipeline.admission().unwrap_err().code(), "persist_degraded");
    assert_eq!(
        pipeline
            .submit(log_event("01J00000000000000000030004", SESSION_A))
            .await
            .unwrap_err()
            .code(),
        "persist_degraded"
    );
}

/// 真实 SQLite：启动自检失败 → 只读；修复 + 重启（新实例 + 自检通过）→ 恢复且 seq 续接。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_store_startup_check_failure_is_recovered_only_by_restart() {
    let dir = TempDir::new().unwrap();
    let (runtime, writer) = start_real_pipeline(&dir, PipelineConfig::default());
    writer
        .submit(log_event("01J00000000000000000040001", SESSION_A))
        .await
        .unwrap();
    writer
        .submit(log_event("01J00000000000000000040002", SESSION_A))
        .await
        .unwrap();

    // 启动自检失败（空间护栏）→ 只读降级。
    let degraded = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(StoreJournal::new(runtime.queue().clone())),
        Arc::new(StoreEventSource::new(runtime.reads().clone())),
        &StartupSelfCheckReport::failing_space(1024),
        &Handle::current(),
    )
    .unwrap();
    assert_eq!(
        degraded.health().storage_state,
        StorageState::PersistDegraded
    );
    assert_eq!(
        degraded
            .submit(log_event("01J00000000000000000040003", SESSION_A))
            .await
            .unwrap_err()
            .code(),
        "persist_degraded"
    );
    let session_a = SessionId::new(SESSION_A).unwrap();
    assert_eq!(
        degraded.readback(&session_a, 0).await.unwrap().events.len(),
        2
    );

    // 修复外部条件 + 重启核心 + 启动自检通过 → 新实例恢复写入，seq = max+1 = 3。
    let resumed = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(StoreJournal::new(runtime.queue().clone())),
        Arc::new(StoreEventSource::new(runtime.reads().clone())),
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &Handle::current(),
    )
    .unwrap();
    let outcome = resumed
        .submit(log_event("01J00000000000000000040004", SESSION_A))
        .await
        .unwrap();
    assert_eq!(outcome.seq(), Some(3), "恢复后 seq 从库中 max+1 续接");
    assert_eq!(resumed.health().storage_state, StorageState::Normal);
    // 旧实例保持只读（P0 无热恢复）。
    assert_eq!(
        degraded.health().storage_state,
        StorageState::PersistDegraded
    );
}

/// 真实 SQLite 兜底语义：`events.id` 主键冲突 → 幂等丢弃；重复 seq → 管线 bug 分类。
#[tokio::test]
async fn real_store_constraint_errors_are_classified() {
    let dir = TempDir::new().unwrap();
    let runtime = open_runtime(&dir);
    let journal = StoreJournal::new(runtime.queue().clone());

    journal
        .append(vec![log_envelope(
            "01J00000000000000000050E01",
            SESSION_A,
            1,
        )])
        .await
        .unwrap();

    // 相同 id（不同 seq）：主键冲突 → DuplicateEventId（幂等命中，不降级）。
    let error = journal
        .append(vec![log_envelope(
            "01J00000000000000000050E01",
            SESSION_A,
            2,
        )])
        .await
        .unwrap_err();
    assert_eq!(error, JournalError::DuplicateEventId);

    // 相同 (session, seq)（不同 id）：UNIQUE 兜底 → DuplicateSeq（管线 bug 信号）。
    let error = journal
        .append(vec![log_envelope(
            "01J00000000000000000050E02",
            SESSION_A,
            1,
        )])
        .await
        .unwrap_err();
    match error {
        JournalError::DuplicateSeq { message } => {
            assert!(
                message.contains("UNIQUE"),
                "需保留 SQLite 原始错误: {message}"
            );
        }
        other => panic!("分类不符: {other:?}"),
    }
}
