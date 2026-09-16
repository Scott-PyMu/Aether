//! M1-04 DoD1：1k 事件写入基准 —— 事务延迟 P95 <50ms（D3 失效条件阈值）；队列深度进诊断。
//!
//! 由 `scripts/test/m1-04/verify-m1-04.mjs` 以 release 运行：
//!   `cargo test --release -p aether-store --test m1_04_bench -- --ignored --nocapture`
//!
//! debug 构建的 bundled SQLite 为 -O0，事务延迟不具代表性，故本基准标记 `#[ignore]`，
//! 默认不随 `cargo test` 执行。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_core::SessionId;
use aether_store::{StoreRuntime, WriteQueueConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "基准：由 verify-m1-04 以 --release 运行"]
async fn dod1_write_benchmark_1000_events_transaction_latency_p95() {
    let dir = common::temp_dir("bench-1k");
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let queue = runtime.queue().clone();

    // 诊断采样：队列深度（DoD1「队列深度进诊断」）。
    let peak_depth = Arc::new(AtomicUsize::new(0));
    let stop_sampler = Arc::new(AtomicBool::new(false));
    let sampler = {
        let queue = queue.clone();
        let peak_depth = Arc::clone(&peak_depth);
        let stop_sampler = Arc::clone(&stop_sampler);
        tokio::spawn(async move {
            while !stop_sampler.load(Ordering::Relaxed) {
                peak_depth.fetch_max(queue.depth(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
        })
    };

    // 写入负载：10 个并发作业 × 100 条 = 1000 条事件（每作业一次 append_events）。
    let mut set = tokio::task::JoinSet::new();
    for job in 0..10u64 {
        let queue = queue.clone();
        set.spawn(async move {
            let started = Instant::now();
            let receipt = queue
                .append_events(common::delta_events("sess-bench", job * 100, 100))
                .await;
            (receipt, started.elapsed())
        });
    }

    let mut receipts = Vec::with_capacity(10);
    let mut e2e = Vec::with_capacity(10);
    while let Some(joined) = set.join_next().await {
        let (receipt, elapsed) = joined.unwrap();
        receipts.push(receipt.unwrap());
        e2e.push(elapsed);
    }
    stop_sampler.store(true, Ordering::Relaxed);
    let _ = sampler.await;

    assert_eq!(receipts.len(), 10);
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.entries)
            .sum::<usize>(),
        1_000
    );

    // 事务延迟样本：按提交时点去重（同一批次的多个作业共享同一次事务）。
    let commit_samples: BTreeSet<(i64, u64, usize)> = receipts
        .iter()
        .map(|receipt| {
            (
                receipt.committed_at_ms,
                receipt.commit_ms,
                receipt.batch_entries,
            )
        })
        .collect();
    let commit_ms: Vec<Duration> = commit_samples
        .iter()
        .map(|(_, commit_ms, _)| Duration::from_millis(*commit_ms))
        .collect();
    let commit_p95 = common::percentile_95(&commit_ms);
    let e2e_p95 = common::percentile_95(&e2e);
    let e2e_max = e2e.iter().max().unwrap();
    let metrics = queue.metrics();

    println!(
        "DoD1 基准：提交 1000 条事件 / {} 个事务批次；事务延迟 P95={commit_p95:?} \
         max={:?}；端到端（提交→落盘回执）P95={e2e_p95:?} max={e2e_max:?}；\
         队列深度峰值={}（容量 {}）；提交条目={} 失败={} 单批最大={}",
        commit_ms.len(),
        commit_ms.iter().max().unwrap(),
        peak_depth.load(Ordering::Relaxed),
        metrics.capacity,
        metrics.committed_entries,
        metrics.failed_entries,
        metrics.max_batch_entries,
    );

    // DoD1 阈值：事务延迟 P95 <50ms（D3 失效条件阈值）；长事务目标 <500ms。
    assert!(
        commit_p95 < Duration::from_millis(50),
        "事务延迟 P95 必须 <50ms，实际 {commit_p95:?}"
    );
    assert!(
        commit_ms
            .iter()
            .all(|sample| *sample < Duration::from_millis(500)),
        "单事务必须 <500ms（D3 长事务禁止）：{commit_ms:?}"
    );
    assert!(
        e2e_p95 < Duration::from_millis(500),
        "端到端 P95 应受控，实际 {e2e_p95:?}"
    );

    // 诊断快照可用性与一致性（队列深度进诊断）。
    assert_eq!(metrics.capacity, 4_096);
    assert_eq!(metrics.depth, 0, "排空后队列深度必须归零");
    assert_eq!(metrics.pending_batch_entries, 0);
    assert_eq!(metrics.committed_entries, 1_000);
    assert_eq!(metrics.failed_entries, 0);
    assert_eq!(metrics.pressure_level, None);

    // 落盘验证：1000 条可读回、seq 唯一。
    let session = SessionId::new("sess-bench").unwrap();
    assert_eq!(runtime.reads().event_count(&session).await.unwrap(), 1_000);
    assert_eq!(runtime.reads().max_seq(&session).await.unwrap(), Some(999));
    let page = runtime
        .reads()
        .events_page(&session, None, 1_000)
        .await
        .unwrap();
    assert_eq!(page.len(), 1_000);
    let distinct: BTreeSet<u64> = page.iter().map(|event| event.seq).collect();
    assert_eq!(distinct.len(), 1_000, "seq 必须唯一");

    runtime.shutdown().await.unwrap();
}
