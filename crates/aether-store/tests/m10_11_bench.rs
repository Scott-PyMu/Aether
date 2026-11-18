//! M10-11（ADR-039 v1.0 D-039-4）删除事务档位矩阵与保留路径基准（release）。
//!
//! 产品路径：`select_retention_batch_generation` → `archive_retention_batch` →
//! `DeleteEventsInShard`（单写队列；单分片事务）。按 500/1000/2000/5000 四档测量
//! 删除事务 P50/P95/max，选「P95 <500ms」的最大档（结果写入
//! `docs/M10-11-metrics.json` 与任务证据）。
//!
//! 运行：`cargo test --release -p aether-store --test m10_11_bench -- --nocapture`
//! 证据目录：`AETHER_M10_11_EVIDENCE_DIR`（未设置仅 stdout）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use aether_store::archive::{
    archive_retention_batch, explain_retention_batch, select_retention_batch_generation,
    verify_archive_file,
};
use aether_store::layout::{initialize as initialize_layout, shard_for, StorageLayout};
use aether_store::{
    generation_capacity, generation_live_bytes, StoreCommand, StoreRuntime, WriteQueueConfig,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const SESSION_SHARD0: &str = "01J8ZQ00000000000000000007";
const OLD_TS: i64 = 1_700_000_000_000;
const CUTOFF: i64 = 1_750_000_000_000;
const ITERATIONS: usize = 10;

fn identity_sanitize(value: &Value) -> Result<String, aether_store::StoreError> {
    serde_json::to_string(value).map_err(|error| aether_store::StoreError::Internal {
        reason: error.to_string(),
    })
}

fn seed_events(path: &Path, rows: usize) {
    let mut connection = Connection::open(path).expect("open shard");
    let transaction = connection.transaction().expect("seed txn");
    for index in 0..rows {
        let seq = u64::try_from(index).unwrap_or(0) + 1;
        transaction
            .execute(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES (?1, ?2, NULL, 'mock', ?3, 'log', ?4, ?5, 1)",
                params![
                    format!("01J8ZQ{seq:020}"),
                    SESSION_SHARD0,
                    i64::try_from(seq).unwrap_or(0),
                    format!(r#"{{"level":"info","message":"retention-bench-{index}"}}"#),
                    OLD_TS
                ],
            )
            .expect("seed event");
    }
    transaction.commit().expect("seed commit");
}

fn percentile(sorted_ms: &[u64], percentile: f64) -> u64 {
    if sorted_ms.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted_ms.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted_ms.len() - 1);
    sorted_ms[index]
}

fn stats(durations: &[u64]) -> Value {
    let mut sorted = durations.to_vec();
    sorted.sort_unstable();
    let sum: u64 = sorted.iter().sum();
    json!({
        "samples": sorted.len(),
        "min_ms": sorted.first().copied().unwrap_or(0),
        "p50_ms": percentile(&sorted, 0.50),
        "p95_ms": percentile(&sorted, 0.95),
        "max_ms": sorted.last().copied().unwrap_or(0),
        "mean_ms": if sorted.is_empty() { 0.0 } else { sum as f64 / sorted.len() as f64 },
    })
}

fn runtime_for(layout: StorageLayout) -> (StoreRuntime, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio");
    let storage =
        StoreRuntime::open_generation(layout, WriteQueueConfig::default(), runtime.handle())
            .expect("open generation runtime");
    (storage, runtime)
}

fn workdir() -> tempfile::TempDir {
    if let Some(root) = std::env::var_os("AETHER_M10_11_BENCH_ROOT") {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).expect("bench root");
        tempfile::Builder::new()
            .prefix("m10-11-")
            .tempdir_in(&root)
            .expect("temp dir in bench root")
    } else {
        tempfile::tempdir().expect("temp")
    }
}

fn measure_tier(tier: usize, external: Option<&StorageLayout>) -> Value {
    let dir = workdir();
    let layout = match external {
        Some(layout) => layout.clone(),
        None => {
            let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
            let shard_id = shard_for(SESSION_SHARD0);
            assert_eq!(shard_id, 0);
            let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
            let seed_rows = tier.saturating_mul(ITERATIONS).saturating_add(tier);
            seed_events(&shard_path, seed_rows);
            layout
        }
    };
    let cutoff = if external.is_some() {
        i64::MAX - 1
    } else {
        CUTOFF
    };
    let shard_id = shard_for(SESSION_SHARD0);
    let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
    let archive_dir = dir.path().join("archives");
    std::fs::create_dir_all(&archive_dir).expect("archive dir");

    let explain = {
        let connection =
            Connection::open_with_flags(&shard_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open read-only");
        explain_retention_batch(&connection, cutoff, tier).expect("explain")
    };

    let (storage, runtime) = runtime_for(layout.clone());
    eprintln!("[m10-11-bench] tier={tier} runtime opened");
    let queue = storage.queue().clone();
    let mut delete_ms = Vec::with_capacity(ITERATIONS);
    let mut select_ms = Vec::with_capacity(ITERATIONS);
    let mut archive_ms = Vec::with_capacity(ITERATIONS);
    let mut archived_rows = 0usize;
    runtime.block_on(async {
        for iteration in 0..ITERATIONS {
            eprintln!("[m10-11-bench] tier={tier} iteration={iteration} select");
            let select_started = Instant::now();
            let batches = select_retention_batch_generation(&layout, cutoff, tier).expect("select");
            select_ms.push(u64::try_from(select_started.elapsed().as_millis()).unwrap_or(u64::MAX));
            assert_eq!(batches.len(), 1, "单分片数据集应返回 1 个批");
            let (batch_shard, batch) = batches.into_iter().next().expect("batch");
            assert_eq!(batch.len(), tier, "批大小必须达到档位");
            archived_rows += batch.len();
            eprintln!("[m10-11-bench] tier={tier} iteration={iteration} archive");
            let archive_started = Instant::now();
            let report = archive_retention_batch(
                &archive_dir,
                &batch,
                1_800_000_000_000,
                &identity_sanitize,
            )
            .expect("archive");
            verify_archive_file(&report.path).expect("verify archive");
            archive_ms
                .push(u64::try_from(archive_started.elapsed().as_millis()).unwrap_or(u64::MAX));
            let rowids: Vec<i64> = batch.iter().map(|row| row.rowid).collect();
            eprintln!("[m10-11-bench] tier={tier} iteration={iteration} delete");
            let delete_started = Instant::now();
            queue
                .execute(StoreCommand::DeleteEventsInShard {
                    shard_id: batch_shard,
                    rowids,
                })
                .await
                .expect("delete batch");
            delete_ms.push(u64::try_from(delete_started.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
        storage.shutdown().await.expect("shutdown");
    });
    let physical = generation_live_bytes(&layout);
    let capacity = generation_capacity(&layout).expect("capacity");
    let delete_stats = stats(&delete_ms);
    let p95 = delete_stats["p95_ms"].as_u64().unwrap_or(u64::MAX);
    json!({
        "tier": tier,
        "delete": delete_stats,
        "select": stats(&select_ms),
        "archive": stats(&archive_ms),
        "archived_rows": archived_rows,
        "explain": explain,
        "physical_bytes": physical,
        "live_bytes": capacity.live_bytes,
        "compliant": p95 < 500,
    })
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m10-11-bench] {name} = {value}");
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
fn retention_delete_batch_tier_matrix() {
    let external = std::env::var_os("AETHER_M10_11_GENERATION").and_then(|path| {
        aether_store::read_layout(Path::new(&path))
            .map_err(|error| {
                eprintln!("[m10-11-bench] AETHER_M10_11_GENERATION 无效：{error}");
                error
            })
            .ok()
    });
    let mut tiers = Vec::new();
    let selected_tiers: Vec<usize> = std::env::var("AETHER_M10_11_TIERS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .collect()
        })
        .filter(|values: &Vec<usize>| !values.is_empty())
        .unwrap_or_else(|| vec![500, 1_000, 2_000, 5_000]);
    for tier in selected_tiers {
        tiers.push(measure_tier(tier, external.as_ref()));
    }
    let chosen = tiers
        .iter()
        .filter(|value| value["compliant"].as_bool() == Some(true))
        .filter_map(|value| value["tier"].as_u64())
        .max()
        .unwrap_or(500);
    let result = json!({
        "task": "M10-11",
        "adr": "ADR-039 v1.0 D-039-4",
        "dataset": if external.is_some() { "standard-generation" } else { "synthetic-50k-per-tier" },
        "tiers": tiers,
        "chosen_tier": chosen,
        "iterations": ITERATIONS,
        "debug_build": cfg!(debug_assertions),
        "hard_threshold_ms": 500,
    });
    write_evidence("delete_batch_matrix", &result);
    assert!(
        tiers
            .iter()
            .any(|value| value["compliant"].as_bool() == Some(true)),
        "至少一档必须满足删除事务 P95 <500ms：{result}"
    );
    assert!(
        (500..=5_000).contains(&chosen),
        "档位必须来自 500/1000/2000/5000 矩阵：{result}"
    );
}
