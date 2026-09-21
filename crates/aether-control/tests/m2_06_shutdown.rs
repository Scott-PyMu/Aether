//! M2-06 控制层集成测试：真实管线 × 存储的关闭序列。
//!
//! 覆盖：
//! - 管线 drain 语义（D2）：放弃未落盘 delta、保留已入队控制事件；
//! - 存储侧五步顺序（shadow 日志）与 `-wal` 0 字节在真实管线上下文中的端到端表现；
//! - 关停后重开：仅控制事件落库、delta 无残迹。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
mod m2_support;

use std::time::Duration;

use aether_control::{PipelineConfig, SubmitOutcome};
use aether_core::{EventType, SessionId};
use aether_store::{ShutdownStep, WriteQueueConfig};
use m2_support::TestCore;

use common::{MESSAGE_1, SESSION_A};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_drain_then_storage_five_step_sequence_end_to_end() {
    // delta 合并窗口放大：delta 保持在内存缓冲，由关闭序列放弃（D2）。
    let core = TestCore::open_with(
        WriteQueueConfig::default(),
        PipelineConfig {
            delta_flush_interval: Duration::from_secs(10),
            ..PipelineConfig::default()
        },
    )
    .await;

    // 控制事件（log）先日志后广播，立即落盘。
    let control = core
        .pipeline
        .submit(common::log_event("01J00000000000000000000L01", SESSION_A))
        .await
        .unwrap();
    assert_eq!(control.seq(), Some(1), "控制事件落盘并获得 seq");
    // delta 进入合并窗口（未落盘、未广播）。
    let delta = core
        .pipeline
        .submit(common::delta_event(
            "01J00000000000000000000D01",
            SESSION_A,
            MESSAGE_1,
            "未落盘 delta",
        ))
        .await
        .unwrap();
    assert_eq!(delta, SubmitOutcome::Buffered, "delta 必须进入合并缓冲");

    // 关闭序列：先关管线（drain 放弃 delta、保留控制事件），再执行存储侧五步。
    core.pipeline.shutdown().await.unwrap();
    assert_eq!(
        core.pipeline.health().delta_buffers_discarded,
        1,
        "关闭序列必须放弃未落盘 delta"
    );
    let closed = core
        .pipeline
        .submit(common::log_event("01J00000000000000000000L02", SESSION_A))
        .await
        .unwrap_err();
    assert_eq!(closed.code(), "pipeline_closed", "关停后提交必须被拒绝");

    // 存储侧关闭序列（drain → 关读连接 → checkpoint(TRUNCATE) → 关写连接 → 退出）。
    let report = core.storage.shutdown().await.unwrap();
    assert!(
        report.matches_d2_order(),
        "shadow 日志顺序必须与 D2 一致：{:?}",
        report.order()
    );
    assert!(report.drained);
    assert!(!report.drain_timed_out);
    assert_eq!(report.read_close.closed, 4);
    let checkpoint = report.checkpoint.as_ref().expect("checkpoint 必须执行");
    assert!(checkpoint.succeeded);
    let checkpoint_step = report.step(ShutdownStep::WalCheckpointTruncate).unwrap();
    assert!(
        checkpoint_step.detail.contains("wal_bytes") && checkpoint_step.detail.contains("→0"),
        "checkpoint 步必须记录 WAL 归零证据：{}",
        checkpoint_step.detail
    );

    // 重开库（模拟下一次启动读取）：仅控制事件落库，delta 无残迹。
    let temp = core.temp;
    let db_path = core.db_path;
    let reopened = m2_support::reopen_core(temp, db_path).await;
    let session = SessionId::new(SESSION_A).unwrap();
    let page = reopened
        .reads()
        .events_page(&session, None, 100)
        .await
        .unwrap();
    assert_eq!(page.len(), 1, "仅控制事件落盘（delta 已放弃）：{page:?}");
    assert_eq!(page[0].event_type(), EventType::Log);

    println!(
        "[m2-06 管线×存储] delta_buffers_discarded=1；pipeline_closed=true；shadow: {}",
        report.shadow_log()
    );
    println!(
        "[m2-06 管线×存储] 重开后事件数=1（仅 log 控制事件）；checkpoint 步={}",
        checkpoint_step.detail
    );

    reopened.pipeline.shutdown().await.unwrap();
    reopened.storage.shutdown().await.unwrap();
}

/// 关停后读查询仍可用（D4 降级期读语义；读连接池关闭后按需惰性重开）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_remain_available_after_storage_shutdown() {
    let core = TestCore::open().await;
    let reads = core.reads();
    core.pipeline
        .submit(common::log_event("01J00000000000000000000R01", SESSION_A))
        .await
        .unwrap();
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();

    let session = SessionId::new(SESSION_A).unwrap();
    let page = reads.events_page(&session, None, 10).await.unwrap();
    assert_eq!(page.len(), 1, "关停后诊断读仍必须可用");
}
