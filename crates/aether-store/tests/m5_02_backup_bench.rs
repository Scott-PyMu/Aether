//! M5-02 DoD2 基准：备份期间写延迟不劣化于 D3 阈值（事务延迟 P95 <50ms；单事务 <500ms）。
//!
//! 机制口径：备份写出走 `create_backup_file_verified`（`VACUUM INTO` 临时文件 +
//! `quick_check` + 原子 rename）在**只读连接**上执行；本基准在同一进程内并发驱动
//! 写队列负载（同 M1-04 DoD1 口径：10 作业 × 100 条事件），断言备份期间写事务
//! 延迟不劣化（ADR-020 §2.1 实现要点 1 / 失效条件）。
//!
//! 由 `scripts/test/m5-02/verify-m5-02.mjs` 以 release 运行：
//!   `cargo test --release -p aether-store --test m5_02_backup_bench -- --ignored --nocapture`
//!
//! debug 构建的 bundled SQLite 为 -O0，事务延迟不具代表性，故本基准标记 `#[ignore]`。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use aether_store::backup::{
    cleanup_backup_temps, create_backup_file_verified, program_schema_version, validate_candidate,
};
use aether_store::{StoreRuntime, WriteQueueConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "基准：由 verify-m5-02 以 --release 运行"]
async fn dod2_write_latency_p95_during_backup() {
    let dir = common::temp_dir("m5-02-bench");
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let reads = runtime.reads().clone();
    let queue = runtime.queue().clone();

    // 种子数据：5k 事件（使 VACUUM INTO 产物具备可观测体积）。
    let seed = queue
        .append_events(common::delta_events("sess-seed", 0, 5_000))
        .await
        .unwrap();
    assert_eq!(seed.entries, 5_000);

    let target = dir.path().join("backups");
    std::fs::create_dir_all(&target).unwrap();
    let dest = target.join("aether-bench.db");

    // 并发：备份（只读连接 VACUUM INTO + 校验 + 原子 rename）× 写负载（1k 事件）。
    let backup = {
        let reads = reads.clone();
        let dest = dest.clone();
        tokio::spawn(async move {
            reads
                .with_connection(move |connection| {
                    create_backup_file_verified(
                        connection,
                        &dest,
                        1_700_000_000_000,
                        "01J0000000000000000000000B0",
                    )
                })
                .await
        })
    };

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
    let backup_bytes = backup.await.unwrap().unwrap();
    assert!(backup_bytes > 0, "备份产物必须写出");

    // 事务延迟样本：按提交时点去重（同一批次的多个作业共享同一次事务；M1-04 口径）。
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
    let metrics = queue.metrics();

    // 产物校验：quick_check + 迁移版本 + 可被恢复候选校验消费；无临时残留。
    let candidate = validate_candidate(&dest, program_schema_version()).unwrap();
    assert_eq!(candidate.schema_version, program_schema_version());
    assert_eq!(cleanup_backup_temps(&target), 0, "成功路径无临时残留");

    println!(
        "M5-02 DoD2 基准：备份期间写入 1000 条事件 / {} 个事务批次；事务延迟 P95={commit_p95:?} \
         max={:?}；端到端 P95={e2e_p95:?}；产物 {} 字节；写失败={}；depth={}",
        commit_ms.len(),
        commit_ms.iter().max().unwrap(),
        backup_bytes,
        metrics.failed_entries,
        metrics.depth,
    );

    // DoD2 阈值：备份期间写事务延迟 P95 <50ms（D3 失效条件阈值）；单事务 <500ms。
    assert!(
        commit_p95 < Duration::from_millis(50),
        "备份期间事务延迟 P95 必须 <50ms（D3），实际 {commit_p95:?}"
    );
    assert!(
        commit_ms
            .iter()
            .all(|sample| *sample < Duration::from_millis(500)),
        "单事务必须 <500ms（D3 长事务禁止）：{commit_ms:?}"
    );
    assert_eq!(metrics.failed_entries, 0, "备份期间不得有写失败");
    assert_eq!(metrics.depth, 0, "排空后队列深度必须归零");

    runtime.shutdown().await.unwrap();
}
