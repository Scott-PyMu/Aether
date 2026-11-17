//! M10-10 启动检查并发矩阵（ADR-039 D-039-3：2/4/8；记录单片/主库/总耗时/失败数）。
//!
//! 默认 `#[ignore]`；用 5.22GiB generation 运行：
//! `AETHER_M10_10_DATA_DIR=<generation> cargo test -p aether-app --test m10_10_concurrency -- --ignored --nocapture`
//! 可用 `AETHER_M10_10_CONCURRENCY=2,4,8` 指定档位。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aether_store::{quick_check_path, read_layout};

fn run_level(paths: &[std::path::PathBuf], concurrency: usize) -> serde_json::Value {
    let cursor = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(vec![None; paths.len()]));
    let failures = Arc::new(AtomicUsize::new(0));
    let wall_started = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..concurrency {
        let cursor = Arc::clone(&cursor);
        let results = Arc::clone(&results);
        let failures = Arc::clone(&failures);
        let paths = paths.to_vec();
        workers.push(std::thread::spawn(move || loop {
            let index = cursor.fetch_add(1, Ordering::AcqRel);
            if index >= paths.len() {
                break;
            }
            let started = Instant::now();
            let report = quick_check_path(&paths[index]);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let ok = matches!(&report, Ok(report) if report.ok);
            if !ok {
                failures.fetch_add(1, Ordering::AcqRel);
            }
            match results.lock() {
                Ok(mut guard) => guard[index] = Some((ms, ok)),
                Err(poisoned) => poisoned.into_inner()[index] = Some((ms, ok)),
            }
        }));
    }
    for worker in workers {
        worker.join().expect("worker");
    }
    let wall_ms = wall_started.elapsed().as_secs_f64() * 1000.0;
    let results = match results.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let per_shard_ms: Vec<Option<f64>> = results
        .iter()
        .map(|entry| entry.map(|(ms, _)| ms))
        .collect();
    let ok_count = results
        .iter()
        .filter(|entry| entry.is_some_and(|(_, ok)| ok))
        .count();
    serde_json::json!({
        "concurrency": concurrency,
        "wall_ms": wall_ms,
        "per_shard_ms": per_shard_ms,
        "ok_count": ok_count,
        "failure_count": failures.load(Ordering::Acquire),
    })
}

#[test]
#[ignore = "M10-10 startup check concurrency matrix (5.22GiB dataset)"]
fn startup_check_concurrency_matrix_records_per_shard_and_failures() {
    let data_dir = std::env::var("AETHER_M10_10_DATA_DIR")
        .unwrap_or_else(|_| "dist-out/m10-09/generation".to_owned());
    let out = std::env::var("AETHER_M10_10_CONCURRENCY_OUT")
        .unwrap_or_else(|_| "../../dist-out/m10-10/concurrency-matrix.json".to_owned());
    let levels: Vec<usize> = std::env::var("AETHER_M10_10_CONCURRENCY")
        .unwrap_or_else(|_| "2,4,8".to_owned())
        .split(',')
        .filter_map(|value| value.trim().parse::<usize>().ok())
        .filter(|value| (1..=8).contains(value))
        .collect();
    assert!(!levels.is_empty(), "至少需要一个合法并发档位");

    let layout = read_layout(Path::new(&data_dir)).expect("layout");
    let paths = layout.shard_paths().to_vec();
    let main_started = Instant::now();
    let main_report = quick_check_path(layout.main_db()).expect("main quick_check");
    let main_ms = main_started.elapsed().as_secs_f64() * 1000.0;
    assert!(main_report.ok, "主库 quick_check 必须通过");

    let mut matrix = Vec::new();
    for concurrency in levels {
        println!("M10-10_CONCURRENCY start concurrency={concurrency}");
        let result = run_level(&paths, concurrency);
        println!(
            "M10-10_CONCURRENCY {}",
            serde_json::to_string(&result).expect("json")
        );
        matrix.push(result);
    }
    let result = serde_json::json!({
        "task": "M10-10",
        "data_dir": data_dir,
        "shard_count": paths.len(),
        "main_ms": main_ms,
        "main_ok": main_report.ok,
        "matrix": matrix,
    });
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write");
    println!(
        "M10-10_CONCURRENCY_MATRIX {}",
        serde_json::to_string(&result).expect("json")
    );
    for entry in result["matrix"].as_array().expect("matrix") {
        assert_eq!(
            entry["failure_count"].as_u64(),
            Some(0),
            "并发矩阵不得有失败样本：{entry}"
        );
        assert_eq!(
            entry["ok_count"].as_u64(),
            Some(paths.len() as u64),
            "并发矩阵必须检查全部分片：{entry}"
        );
    }
}
