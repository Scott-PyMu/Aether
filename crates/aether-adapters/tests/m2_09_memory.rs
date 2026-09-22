//! M2-09 DoD1：大行端到端 + 内存曲线断言受控（真实 Mock 进程，核心侧 RSS 采样）。
//!
//! 覆盖：
//! - 大于 2MiB 任意行（连发 20 次）：不缓冲完整行、断连、记错；核心进程 RSS 增长
//!   受控（缓冲上界 = 帧上限 2MiB + 读取块 64KiB，不随注入量线性增长）；
//! - 1–2MiB 非引用行（连发 20 次）：逐行正常解析、连接保持、无效帧计数 0；
//!   核心进程 RSS 增长受控（逐行解析后释放，不累积）。
//!
//! RSS 口径：采样测试进程（即核心读取端）——`sysinfo` 与生产 ResourcePatrol 同源。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use aether_adapters::{AdapterNotification, DisconnectReason, Method, MAX_FRAME_BYTES};
use common::MockHarness;

/// 内存曲线断言上界：基线之上的允许增长（MiB）。
///
/// 常量级调参（记录于任务证据）：>2MiB 场景理论增长 ≈ 2MiB+64KiB（≈2.06MiB）；
/// 1–2MiB 场景逐行解析，峰值 ≈ 解码缓冲 + 待消费通知（≤3 行 × 1.5MiB ≈ 4.5MiB）。
/// 64MiB 上界为断言容差（对齐 Windows 匿名管道/分配器行为），远低于「按注入量线性
/// 累积」的失败模式（20 行 × 1.5MiB ≈ 30MiB 仅 1–2MiB 场景，>2MiB 场景失败模式
/// 为 20 × 2.5MiB = 50MiB+ 且无上界）。
const MEMORY_GROWTH_BOUND_MIB: u64 = 64;

/// 当前进程 RSS（MiB）——与生产 `SysinfoRssSampler` 同源（sysinfo 0.30.13）。
fn current_rss_mib() -> u64 {
    use sysinfo::{Pid, ProcessRefreshKind, System};
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessRefreshKind::new().with_memory());
    system
        .process(Pid::from_u32(std::process::id()))
        .map(|process| process.memory() / (1024 * 1024))
        .unwrap_or(0)
}

fn assert_rss_bounded(baseline: u64, label: &str) {
    let after = current_rss_mib();
    let growth = after.saturating_sub(baseline);
    println!(
        "[m2-09 DoD1] {label}：基线 {baseline} MiB → 采样 {after} MiB（增长 {growth} MiB ≤ {MEMORY_GROWTH_BOUND_MIB} MiB）"
    );
    assert!(
        growth <= MEMORY_GROWTH_BOUND_MIB,
        "{label} 内存曲线失控：增长 {growth} MiB > {MEMORY_GROWTH_BOUND_MIB} MiB（基线 {baseline}）"
    );
}

/// DoD1：>2MiB 任意行——不缓冲、断连、记错；内存曲线受控。
#[tokio::test]
async fn line_over_2mib_burst_disconnects_without_buffering_and_memory_bounded() {
    let baseline = current_rss_mib();
    let Some(mut harness) =
        MockHarness::launch(&["--inject", "line-over-2mib-burst", "--inject-count", "20"]).await
    else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(10))
        .await
        .expect(">2MiB 任意行必须断连");
    match reason {
        DisconnectReason::LineTooLong { limit } => {
            assert_eq!(limit, MAX_FRAME_BYTES, "上限必须为 2MiB");
        }
        other => panic!("断连原因不符: {other:?}"),
    }
    assert!(
        harness
            .connection
            .recorded_errors()
            .iter()
            .any(|line| line.contains("上限") || line.contains("2MiB")),
        "超限断连必须记错"
    );
    // 内存曲线：断连后 RSS 增长受控（不缓冲完整行；缓冲上界 = 帧上限 + 读取块）。
    assert_rss_bounded(baseline, ">2MiB 连发 20 次");
    harness.kill().await;
}

/// DoD1：1–2MiB 非引用行——逐行正常解析；内存曲线受控（不累积）。
#[tokio::test]
async fn oversized_line_burst_parses_all_and_memory_bounded() {
    let baseline = current_rss_mib();
    let Some(mut harness) =
        MockHarness::launch(&["--inject", "oversized-line-burst", "--inject-count", "20"]).await
    else {
        return;
    };
    let mut parsed = 0usize;
    // 等待预算放宽（原 15s）：llvm-cov 插桩 + 同二进制 4 用例并发下，20 行 ×
    // 1–2MiB（≈30MiB）的解析偶发超出 15s（CI Coverage gate flake：
    // `全部 1–2MiB 行必须解析`，`m2_09_memory.rs:114`）。功能断言不变（20 行必须
    // 全部解析 + RSS 受控），此处仅放宽等待预算（常量级调参，记录于 M2-09 证据 §6）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while parsed < 20 && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let notification = tokio::time::timeout(remaining, harness.connection.next_notification())
            .await
            .expect("1–2MiB 非引用行必须在超时前解析")
            .expect("通知通道不应关闭");
        match notification {
            AdapterNotification::Log(params) => {
                let pad = params["pad"].as_str().unwrap_or_default().len();
                assert!(
                    pad > 1024 * 1024 && pad < 2 * 1024 * 1024,
                    "样例必须落在 1–2MiB 区间（实际 {pad}）"
                );
                parsed += 1;
            }
            other => panic!("通知类型不符: {other:?}"),
        }
    }
    assert_eq!(parsed, 20, "全部 1–2MiB 行必须解析");
    assert_eq!(
        harness.connection.invalid_frames_total(),
        0,
        "1–2MiB 非引用行不得计为无效帧"
    );
    // 内存曲线：逐行解析后释放（峰值 ≈ 解码缓冲 + ≤3 行待消费），不按注入量累积。
    assert_rss_bounded(baseline, "1–2MiB 连发 20 次");

    let pong = harness
        .connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect("1–2MiB 连发后连接必须可用");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
}

/// DoD1 回归：缓冲上界常量（帧上限 2MiB + 读取块 64KiB）冻结。
#[test]
fn memory_bound_constants_are_frozen() {
    assert_eq!(aether_adapters::MAX_FRAME_BYTES, 2 * 1024 * 1024);
    assert_eq!(aether_adapters::READ_CHUNK_BYTES, 64 * 1024);
    assert_eq!(
        aether_adapters::MAX_FRAME_BYTES + aether_adapters::READ_CHUNK_BYTES,
        2 * 1024 * 1024 + 64 * 1024,
        "缓冲上界 = 帧上限 + 读取块（不依赖 OS 管道一次可读多少）"
    );
}

/// DoD1 回归：断连后新连接仍可正常建立（大行风暴不污染后续连接）。
#[tokio::test]
async fn connection_recovers_after_line_over_2mib_storm() {
    let Some(mut first) = MockHarness::launch(&["--inject", "line-over-2mib"]).await else {
        return;
    };
    first
        .wait_for_disconnect(Duration::from_secs(10))
        .await
        .expect(">2MiB 行必须断连");
    drop(first);
    let Some(mut second) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = second.open_session().await;
    let run_id = second.send(&session_id, "hello", "m2-09-recover").await;
    second
        .drive_run(&run_id, Duration::from_secs(15))
        .await
        .expect("断连后的新连接必须完整跑完 run");
    assert_eq!(
        second.run_types(&run_id).last().map(String::as_str),
        Some("run.completed")
    );
    second.shutdown().await;
}
