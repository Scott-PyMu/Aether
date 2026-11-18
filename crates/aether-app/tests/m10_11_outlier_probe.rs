//! M10-11 删除长尾探针（ADR-039 D-039-4 排查用；release + 5.22GiB generation 副本）。
//!
//! 目的：区分删除命令的**队列等待**与**写任务事务内耗时**，定位全量清理中偶发
//! `max_delete_ms` 长尾（示例 8573ms）是否违反 D3 单事务 <500ms。
//!
//! 运行：
//! `AETHER_M10_11_GENERATION=<copy> AETHER_M10_11_PROBE_CHUNKS=1000 \
//!  cargo test --release -p aether-app --test m10_11_outlier_probe -- --ignored --nocapture`
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Instant;

use aether_store::archive::{
    archive_retention_batch, select_retention_batch_generation, verify_archive_file,
};
use aether_store::checkpoint::CheckpointConfig;
use aether_store::layout::read_existing as read_layout;
use aether_store::{StoreCommand, StoreRuntime, WriteQueueConfig};
use serde_json::{json, Value};

fn identity_sanitize(value: &Value) -> Result<String, aether_store::StoreError> {
    serde_json::to_string(value).map_err(|error| aether_store::StoreError::Internal {
        reason: error.to_string(),
    })
}

fn stats(values: &[u64]) -> Value {
    if values.is_empty() {
        return json!({"samples": 0});
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p = |q: f64| -> u64 {
        let rank = (q * sorted.len() as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    };
    let sum: u64 = sorted.iter().sum();
    json!({
        "samples": sorted.len(),
        "min_ms": sorted.first().copied().unwrap_or(0),
        "p50_ms": p(0.50),
        "p95_ms": p(0.95),
        "p99_ms": p(0.99),
        "max_ms": sorted.last().copied().unwrap_or(0),
        "mean_ms": sum as f64 / sorted.len() as f64,
    })
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m10-11-probe] {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M10_11_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(dir.join(format!("{name}.json")), text);
    }
}

#[test]
#[ignore = "需要 5.22GiB generation 副本（AETHER_M10_11_GENERATION）；release"]
fn delete_long_tail_probe() {
    let source = std::env::var_os("AETHER_M10_11_GENERATION")
        .expect("需要 AETHER_M10_11_GENERATION 指向 generation 副本");
    let layout = read_layout(std::path::Path::new(&source)).expect("generation layout");
    let max_chunks: usize = std::env::var("AETHER_M10_11_PROBE_CHUNKS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    let tier: usize = std::env::var("AETHER_M10_11_PROBE_TIER")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_000);
    let checkpoint_each: bool = std::env::var("AETHER_M10_11_PROBE_CHECKPOINT")
        .map(|value| value == "1")
        .unwrap_or(false);
    let archive_dir = layout.data_dir().join("archives-probe");
    std::fs::create_dir_all(&archive_dir).expect("archive dir");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio");
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("open generation");
    let queue = storage.queue().clone();

    let mut queue_ms = Vec::new();
    let mut tx_ms = Vec::new();
    let mut slow = Vec::new();
    let mut chunks = 0usize;
    let cutoff = i64::MAX - 1;
    runtime.block_on(async {
        'outer: loop {
            let batches =
                select_retention_batch_generation(&layout, cutoff, 5_000).expect("select batch");
            if batches.is_empty() {
                break;
            }
            for (shard_id, batch) in batches {
                for chunk in batch.chunks(tier) {
                    if chunks >= max_chunks {
                        break 'outer;
                    }
                    let rows = chunk.to_vec();
                    let report = archive_retention_batch(
                        &archive_dir,
                        &rows,
                        1_800_000_000_000i64.saturating_add(chunks as i64),
                        &identity_sanitize,
                    )
                    .expect("archive");
                    verify_archive_file(&report.path).expect("verify archive");
                    let rowids: Vec<i64> = rows.iter().map(|row| row.rowid).collect();
                    let started = Instant::now();
                    queue
                        .execute(StoreCommand::DeleteEventsInShard { shard_id, rowids })
                        .await
                        .expect("delete");
                    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let tx = queue.metrics().last_commit_ms;
                    queue_ms.push(elapsed);
                    tx_ms.push(tx);
                    if elapsed >= 500 || tx >= 500 {
                        slow.push(json!({
                            "chunk": chunks,
                            "queue_ms": elapsed,
                            "tx_ms": tx,
                            "main_wal_bytes": storage.wal_size_bytes(),
                        }));
                    }
                    if checkpoint_each {
                        storage
                            .checkpoint_truncate(CheckpointConfig::default())
                            .await
                            .expect("checkpoint");
                    }
                    chunks += 1;
                }
            }
            tokio::task::yield_now().await;
        }
        storage.shutdown().await.expect("shutdown");
    });

    let result = json!({
        "task": "M10-11",
        "scenario": "delete_long_tail_probe",
        "generation": source.to_string_lossy(),
        "max_chunks": max_chunks,
        "tier": tier,
        "checkpoint_each_chunk": checkpoint_each,
        "chunks": chunks,
        "queue": stats(&queue_ms),
        "tx": stats(&tx_ms),
        "slow_count": slow.len(),
        "slow": slow,
        "d3_hard_threshold_ms": 500,
    });
    write_evidence("delete_long_tail_probe", &result);
    assert!(chunks > 0, "探针必须执行至少一个删除块");
}
