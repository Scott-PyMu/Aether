//! M10-10 启动预算复测（5.22GiB generation；后台并行 quick_check）。
//!
//! 该测试不依赖 Electron，只测量 core 启动/检查；release Electron 证据由
//! `scripts/test/m10-10/startup-e2e.mjs` 负责。默认 `#[ignore]`：
//! `AETHER_M10_10_DATA_DIR=<generation> cargo test -p aether-app --test m10_10_startup -- --ignored --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use aether_app::core_health::{boot_core_full, startup_check_profile, StaticRuntimeSummaries};
use aether_store::{read_layout, SHARD_COUNT};

#[test]
#[ignore = "M10-10 5.22GiB startup benchmark"]
fn startup_defers_quick_check_and_completes_within_budget() {
    let data_dir = std::env::var("AETHER_M10_10_DATA_DIR")
        .unwrap_or_else(|_| "dist-out/m10-09/generation".to_owned());
    let out = std::env::var("AETHER_M10_10_OUT")
        .unwrap_or_else(|_| "../../dist-out/m10-10/startup-core.json".to_owned());
    let layout = read_layout(Path::new(&data_dir)).expect("generation layout");
    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate sidecar");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio");
    let started = Instant::now();
    let boot = boot_core_full(
        Path::new(&data_dir),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("boot core");
    let initialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    let initial_check = boot.storage_check.snapshot();
    boot.storage_check.request_start();
    let check_started = Instant::now();
    runtime
        .block_on(boot.storage_check_task)
        .expect("storage check task");
    let storage_check_ms = check_started.elapsed().as_secs_f64() * 1000.0;
    let report = boot.backend.report();
    let check = report.storage_check.expect("storage_check");
    assert_eq!(check.state, "ok", "启动检查必须通过");
    assert_eq!(check.write_gate, "open");
    assert!(check.main_checked);
    assert_eq!(check.checked_shards, SHARD_COUNT);
    assert_eq!(check.total_shards, SHARD_COUNT);
    let diagnostics = boot.storage_check.diagnostics();
    assert_eq!(diagnostics.failure_count, 0);
    assert_eq!(diagnostics.shards.len(), SHARD_COUNT);
    assert!(diagnostics.main_ms.is_some(), "必须记录主库检查耗时");
    assert!(diagnostics.main_ok.unwrap_or(false), "主库检查必须通过");
    let profile = startup_check_profile(Path::new(&data_dir));
    assert_eq!(diagnostics.device, profile.device.code());
    assert_eq!(diagnostics.concurrency, profile.concurrency);
    assert!(
        (2..=8).contains(&profile.concurrency),
        "并发必须落在 ADR-039 范围 2..=8：{profile:?}"
    );
    assert!(
        initial_check.state == "checking" || initial_check.state == "ok",
        "返回 health 时检查状态必须可观测：{}",
        initial_check.state
    );
    assert!(
        initial_check.write_gate == "checking" || initial_check.write_gate == "open",
        "write_gate 必须为 checking/open：{}",
        initial_check.write_gate
    );

    // 正常关闭写 sidecar；第二次启动应严格匹配并跳过 quick_check。
    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let second_started = Instant::now();
    let second = boot_core_full(
        Path::new(&data_dir),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("second boot");
    let second_initialize_ms = second_started.elapsed().as_secs_f64() * 1000.0;
    let second_task_started = Instant::now();
    runtime
        .block_on(second.storage_check_task)
        .expect("second storage check");
    let second_check_ms = second_task_started.elapsed().as_secs_f64() * 1000.0;
    let second_report = second.backend.report();
    let second_check = second_report.storage_check.expect("second storage_check");
    assert_eq!(second_check.state, "ok");
    assert_eq!(second_check.write_gate, "open");
    // ADR-039 D-039-2 / P2-3：干净启动且 sidecar 全匹配时 clean_start_count +1。
    let second_storage = second.storage.take().expect("second storage");
    assert_eq!(
        second_storage.clean_start_count(),
        1,
        "严格 sidecar 启动必须使 clean_start_count=1"
    );
    runtime
        .block_on(second_storage.shutdown())
        .expect("second shutdown");

    // 严格 sidecar 负例：background_check_completed=false（异常/未完成检查）必须失效。
    let mut sidecar = aether_store::read_clean_shutdown(&layout).expect("sidecar");
    sidecar.background_check_completed = false;
    aether_store::write_clean_shutdown(&layout, &sidecar).expect("write invalid sidecar");
    assert!(
        !aether_store::clean_shutdown_is_strict(&layout, env!("CARGO_PKG_VERSION")),
        "background_check_completed=false 必须触发后台检查"
    );
    sidecar.background_check_completed = true;
    aether_store::write_clean_shutdown(&layout, &sidecar).expect("write valid sidecar");
    assert!(
        aether_store::clean_shutdown_is_strict(&layout, env!("CARGO_PKG_VERSION")),
        "恢复有效 sidecar 后应严格匹配"
    );
    // WAL 非 0：严格 sidecar 必须失效。
    let wal = aether_store::sqlite_sidecar(layout.main_db(), "-wal");
    std::fs::write(&wal, b"wal-not-checkpointed").expect("write wal");
    assert!(
        !aether_store::clean_shutdown_is_strict(&layout, env!("CARGO_PKG_VERSION")),
        "WAL 非 0 必须触发后台检查"
    );
    let _ = std::fs::remove_file(&wal);

    let result = serde_json::json!({
        "task": "M10-10",
        "data_dir": data_dir,
        "initializeMs": initialize_ms,
        "storageCheckMs": storage_check_ms,
        "initialStorageCheck": initial_check,
        "storageCheck": check,
        "secondInitializeMs": second_initialize_ms,
        "secondStorageCheckMs": second_check_ms,
        "secondStorageCheck": second_check,
        "storageCheckDiagnostics": diagnostics,
        "startupCheckProfile": profile,
        "budgets": { "initializeMs": 1500, "storageCheckMs": 30000 }
    });
    if let Some(parent) = Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write result");
    println!(
        "M10-10_STARTUP {}",
        serde_json::to_string(&result).expect("json")
    );
    assert!(
        initialize_ms <= 1500.0,
        "initializeMs {initialize_ms} > 1500"
    );
    assert!(
        storage_check_ms <= 30_000.0,
        "storageCheckMs {storage_check_ms} > 30000"
    );
    assert!(
        second_initialize_ms <= 1500.0,
        "secondInitializeMs {second_initialize_ms} > 1500"
    );
    assert!(
        second_check_ms <= 1500.0,
        "strict sidecar 第二次检查 {second_check_ms} > 1500"
    );
}
