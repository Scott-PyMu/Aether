//! M2-07 DoD6 集成测试：P0 运行期日志汇聚端（ADR-007 §5-1）。
//!
//! 断言口径：
//! - `tracing_subscriber` 全局订阅器 → [`LogSink`]（环形缓冲 + 文件）；
//! - 真实存储写失败（写队列关闭）触发 `attempt=1/3`…`3/3` 与 `persist_degraded` 诊断，
//!   均可经环形缓冲（`export_text`，M3-05 诊断包导出源）与日志文件读取；
//! - `persist_degraded` 进入路径的 `tracing::warn!` 被完整捕获。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use aether_control::{
    EventPipeline, PipelineConfig, StartupSelfCheckReport, StoreEventSource, StoreJournal,
};
use aether_store::{StoreRuntime, WriteQueueConfig};
use aether_tauri::logging::{global_sink, LogSink};
use serde_json::json;

/// 日志汇聚端捕获写失败重试口径（attempt=n/3）与 persist_degraded 诊断。
#[test]
fn runtime_log_aggregation_captures_persist_attempts_and_degraded() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_path = dir.path().join("logs").join("aether.log");

    // 1) 接线：环形缓冲 + 文件（生产同路径形态 `<data_dir>/logs/aether.log`）。
    let sink = LogSink::with_file(128, &log_path).expect("日志汇聚端");
    aether_tauri::logging::init(Arc::clone(&sink)).expect("安装全局订阅器");
    assert!(global_sink().is_some(), "全局汇聚端句柄必须可用");

    // 2) 真实存储 + 管线；关闭写队列 → 连续 3 次写事务尝试失败 → persist_degraded。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio 运行时");
    let handle = runtime.handle().clone();
    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("打开存储");
    let pipeline = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(StoreJournal::new(storage.queue().clone())),
        Arc::new(StoreEventSource::new(storage.reads().clone())),
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &handle,
    )
    .expect("启动管线");

    // 关闭写队列（写任务 drain 后退出）→ 后续 append 必失败。
    runtime.block_on(storage.shutdown()).expect("关停存储");

    let envelope = json!({
        "v": 1,
        "id": "01J000000000000000000L0G01",
        "session_id": "01J000000000000000000L0G00",
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 1,
        "type": "log",
        "payload": { "level": "info", "message": "日志汇聚端 DoD6" }
    });
    let error = runtime
        .block_on(pipeline.submit(envelope))
        .expect_err("写队列关闭后必须失败");
    assert_eq!(error.code(), "persist_degraded");

    // 3) 环形缓冲（诊断包导出源）必须完整捕获 attempt=n/3 与降级诊断。
    let exported = sink.export_text();
    for needle in [
        "attempt=1/3",
        "attempt=2/3",
        "attempt=3/3",
        "进入 persist_degraded",
    ] {
        assert!(
            exported.contains(needle),
            "环形缓冲必须捕获 {needle}；实际：\n{exported}"
        );
    }
    assert_eq!(
        exported.matches("attempt=1/3").count(),
        1,
        "第 1 次尝试必须恰好记录一次（含首次；ADR-007 决策 2）"
    );
    assert_eq!(exported.matches("attempt=2/3").count(), 1);
    assert_eq!(exported.matches("attempt=3/3").count(), 1);
    assert_eq!(
        exported.matches("attempt=4/3").count(),
        0,
        "不得出现第 4 次尝试（MAX_WRITE_ATTEMPTS=3 含首次）"
    );

    // 4) 文件产物同口径（运行期可外部查看；M3-05 诊断包可打包）。
    let file_text = std::fs::read_to_string(&log_path).expect("读取日志文件");
    for needle in ["attempt=1/3", "attempt=3/3", "persist_degraded"] {
        assert!(
            file_text.contains(needle),
            "日志文件必须包含 {needle}；实际：\n{file_text}"
        );
    }
    assert!(sink.line_count() >= 3, "环形缓冲必须记录全部尝试日志");

    println!(
        "[m2-07 DoD6] 日志汇聚端：attempt 行={}；文件={:?}；导出片段：\n{}",
        exported.matches("attempt=").count(),
        sink.file_path().map(|path| path.display().to_string()),
        exported
            .lines()
            .filter(|line| line.contains("attempt=") || line.contains("persist_degraded"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
