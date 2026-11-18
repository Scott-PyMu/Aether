//! M10-14 / ADR-040 §4.3：dirty 1/2/4/8 选择性启动性能单元（release daemon 启动路径）。
//!
//! 由 `scripts/test/m10-14/perf-matrix.mjs` 驱动（复制 5.22 GiB generation 后调用）。
//! 手动复跑：
//! `AETHER_M10_14_DATA_DIR=<generation> AETHER_M10_14_DIRTY=1,2 AETHER_M10_14_COLD=1 \
//!  cargo test --release -p aether-app --test m10_14_perf -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, StaticRuntimeSummaries,
};
use aether_store::{
    read_clean_shutdown, read_layout, write_clean_shutdown, StoreRuntime, WriteQueueConfig,
};

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

#[test]
#[ignore = "M10-14 5.22GiB selective startup matrix"]
fn selective_startup_perf_cell() {
    let data_dir = std::env::var("AETHER_M10_14_DATA_DIR").expect("AETHER_M10_14_DATA_DIR");
    let dirty: Vec<u8> = std::env::var("AETHER_M10_14_DIRTY")
        .unwrap_or_else(|_| "1".to_owned())
        .split(',')
        .filter_map(|value| value.trim().parse::<u8>().ok())
        .collect();
    let cold = std::env::var("AETHER_M10_14_COLD").ok().as_deref() == Some("1");
    let main_dirty = std::env::var("AETHER_M10_14_MAIN_DIRTY").ok().as_deref() == Some("1");
    let run_index = std::env::var("AETHER_M10_14_RUN").unwrap_or_else(|_| "1".to_owned());
    let out = std::env::var("AETHER_M10_14_OUT")
        .unwrap_or_else(|_| "dist-out/m10-14/perf-cell.json".to_owned());

    let layout = read_layout(Path::new(&data_dir)).expect("layout");
    let total_bytes: u64 = layout
        .all_db_paths()
        .iter()
        .map(|path| aether_store::file_size(path))
        .sum();
    let main_bytes = aether_store::file_size(layout.main_db());
    let mut sidecar = read_clean_shutdown(&layout).expect("clean sidecar (prepare first)");
    sidecar.format_version = 1;
    sidecar.background_check_completed = true;
    sidecar.last_full_check_at = Some(now_ms());
    sidecar.clean_start_count = 0;
    if main_dirty {
        sidecar.main.header_change_counter = sidecar.main.header_change_counter.saturating_add(1);
    }
    for id in &dirty {
        if let Some(entry) = sidecar
            .shards
            .iter_mut()
            .find(|entry| entry.shard_id == *id)
        {
            entry.database.header_change_counter =
                entry.database.header_change_counter.saturating_add(1);
        }
    }
    write_clean_shutdown(&layout, &sidecar).expect("dirty sidecar");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let started = Instant::now();
    let boot = boot_core_full_with_options(
        Path::new(&data_dir),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        CoreBootOptions {
            scheduler: SchedulerConfig {
                enabled: false,
                background_delay: Duration::from_secs(3600),
                ..SchedulerConfig::default()
            },
            ..CoreBootOptions::default()
        },
    )
    .expect("boot");
    let initialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    boot.storage_check.request_start();
    runtime
        .block_on(boot.storage_check_task)
        .expect("startup check task");
    let startup_check_ms = started.elapsed().as_secs_f64() * 1000.0;
    let report = boot.backend.report();
    let check = report.storage_check.expect("storage_check");
    let diagnostics = boot.storage_check.diagnostics();
    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");

    let result = serde_json::json!({
        "task": "M10-14",
        "cell": {
            "dirty": dirty,
            "cold": cold,
            "run": run_index,
            "initializeMs": initialize_ms,
            "startupCheckCompletedAtMs": startup_check_ms,
            "checked_shards": check.checked_shards,
            "planned_shards": check.planned_shards,
            "skipped_shards": check.skipped_shards,
            "full_coverage": check.full_coverage,
            "state": check.state,
            "write_gate": check.write_gate,
            "mainMs": diagnostics.main_ms,
            "shardMs": diagnostics.shards,
            "failureCount": diagnostics.failure_count,
            "device": diagnostics.device,
            "concurrency": diagnostics.concurrency,
            "totalBytes": total_bytes,
            "mainBytes": main_bytes,
            "mainRatio": if total_bytes == 0 { 0.0 } else { main_bytes as f64 / total_bytes as f64 },
            "targetMs": 25000,
            "hardMs": 30000
        }
    });
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write");
    println!(
        "M10-14_PERF {}",
        serde_json::to_string(&result).expect("json")
    );
    assert_eq!(check.state, "ok", "启动检查必须通过");
    assert_eq!(check.write_gate, "open");
    assert!(check.main_checked);
    assert_eq!(check.planned_shards, dirty.len());
}

#[test]
#[ignore = "M10-14 prepare clean sidecar for perf copy"]
fn prepare_clean_sidecar() {
    let data_dir = std::env::var("AETHER_M10_14_DATA_DIR").expect("AETHER_M10_14_DATA_DIR");
    let layout = read_layout(Path::new(&data_dir)).expect("layout");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");
    println!("M10-14_PREPARED {}", data_dir);
}

/// 周期完整 `integrity_check` 基线（顺序执行；保守上界，用于 30min 超时核对）。
#[test]
#[ignore = "M10-14 periodic integrity_check profile"]
fn periodic_integrity_check_profile() {
    let data_dir = std::env::var("AETHER_M10_14_DATA_DIR").expect("AETHER_M10_14_DATA_DIR");
    let runs = std::env::var("AETHER_M10_14_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3);
    let out = std::env::var("AETHER_M10_14_OUT")
        .unwrap_or_else(|_| "dist-out/m10-14/integrity-profile.json".to_owned());
    let layout = read_layout(Path::new(&data_dir)).expect("layout");
    let mut samples = Vec::with_capacity(runs);
    let mut failures = 0usize;
    for _ in 0..runs {
        let started = Instant::now();
        for path in layout.all_db_paths() {
            match aether_store::integrity_check_path(&path) {
                Ok(report) if report.ok => {}
                _ => failures = failures.saturating_add(1),
            }
        }
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let result = serde_json::json!({
        "task": "M10-14",
        "check": "integrity_check sequential all dbs",
        "runs": runs,
        "samplesMs": samples,
        "failures": failures,
        "totalBytes": layout.all_db_paths().iter().map(|path| aether_store::file_size(path)).sum::<u64>(),
        "periodicCheckTimeoutMs": 30 * 60 * 1000
    });
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write");
    println!(
        "M10-14_INTEGRITY {}",
        serde_json::to_string(&result).expect("json")
    );
    assert_eq!(failures, 0);
}
