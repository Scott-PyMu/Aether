//! M10-10 storageCheck 波动诊断（非门禁；输出 per-shard 顺序/并行 quick_check 耗时）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::Instant;

use aether_store::{quick_check_path, read_layout};

#[test]
#[ignore = "M10-10 quick_check variance diagnosis"]
fn diagnose_quick_check_variance() {
    let data_dir = std::env::var("AETHER_M10_10_DATA_DIR")
        .unwrap_or_else(|_| "dist-out/m10-09/generation".to_owned());
    let out = std::env::var("AETHER_M10_10_DIAG_OUT")
        .unwrap_or_else(|_| "dist-out/m10-10/check-diag.json".to_owned());
    let layout = read_layout(Path::new(&data_dir)).expect("layout");
    let paths = layout.shard_paths().to_vec();
    let sizes: Vec<u64> = paths
        .iter()
        .map(|path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect();

    let mut sequential = Vec::new();
    for _pass in 0..2 {
        let mut pass_times = Vec::new();
        for path in &paths {
            let started = Instant::now();
            let report = quick_check_path(path).expect("quick_check");
            assert!(report.ok);
            pass_times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        sequential.push(pass_times);
    }

    let barrier = Arc::new(Barrier::new(paths.len()));
    let wall_started = Instant::now();
    let mut handles = Vec::new();
    for path in paths.clone() {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let started = Instant::now();
            let report = quick_check_path(&path).expect("quick_check");
            assert!(report.ok);
            started.elapsed().as_secs_f64() * 1000.0
        }));
    }
    let parallel: Vec<f64> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect();
    let parallel_wall_ms = wall_started.elapsed().as_secs_f64() * 1000.0;

    let result = serde_json::json!({
        "task": "M10-10",
        "data_dir": data_dir,
        "shard_sizes": sizes,
        "sequential_passes_ms": sequential,
        "parallel_per_shard_ms": parallel,
        "parallel_wall_ms": parallel_wall_ms,
        "note": "顺序两轮用于区分冷/热缓存；并行与产品 8 并发检查同口径"
    });
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write");
    println!("M10-10_CHECK_DIAG {}", serde_json::to_string(&result).expect("json"));
}
