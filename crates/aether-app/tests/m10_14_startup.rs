//! M10-14 / ADR-040 D-040-1/D-040-4：选择性启动集成（health v2 字段/实际检查范围）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, StaticRuntimeSummaries,
};
use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    read_clean_shutdown, write_clean_shutdown, StorageLayout, StoreRuntime, WriteQueueConfig,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn clean_layout(dir: &Path, runtime: &tokio::runtime::Runtime) -> StorageLayout {
    let layout = initialize_layout(dir, "0.1.0", 1).expect("init");
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");
    layout
}

fn set_last_full_now(layout: &StorageLayout) {
    let mut sidecar = read_clean_shutdown(layout).expect("sidecar");
    sidecar.last_full_check_at = Some(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("ms"),
    );
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn mark_dirty(layout: &StorageLayout, ids: &[u8]) {
    let mut sidecar = read_clean_shutdown(layout).expect("sidecar");
    for id in ids {
        if let Some(entry) = sidecar
            .shards
            .iter_mut()
            .find(|entry| entry.shard_id == *id)
        {
            entry.database.header_change_counter =
                entry.database.header_change_counter.saturating_add(1);
        }
    }
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn boot_options() -> CoreBootOptions {
    CoreBootOptions {
        scheduler: SchedulerConfig {
            enabled: false,
            background_delay: Duration::ZERO,
            ..SchedulerConfig::default()
        },
        ..CoreBootOptions::default()
    }
}

#[test]
fn selective_startup_checks_main_and_dirty_shards_only() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_last_full_now(&layout);
    mark_dirty(&layout, &[1, 6]);

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        boot_options(),
    )
    .expect("boot");
    let report = boot.backend.report();
    let check = report.storage_check.expect("storage_check");
    assert_eq!(check.contract_version, 2);
    assert_eq!(check.state, "ok");
    assert_eq!(check.mode, "startup");
    assert_eq!(check.write_gate, "open");
    assert!(check.main_checked, "选择性启动必须检查主库");
    assert_eq!(check.planned_shards, 2);
    assert_eq!(check.skipped_shards, 6);
    assert_eq!(check.checked_shards, 2);
    assert!(!check.full_coverage, "选择性启动不是全量覆盖");

    let diagnostics = boot.storage_check.diagnostics();
    assert_eq!(diagnostics.failure_count, 0);
    assert!(diagnostics.main_ms.is_some());
    let mut shards: Vec<u8> = diagnostics
        .shards
        .iter()
        .map(|entry| entry.shard_id)
        .collect();
    shards.sort_unstable();
    assert_eq!(shards, vec![1, 6], "只检查指纹变化分片");

    // 选择性启动通过不得置全量覆盖标志（后台全量未完成）。
    let storage = boot.storage.take().expect("storage");
    assert!(
        !storage.background_check_completed(),
        "选择性启动通过不得置 background_check_completed=true"
    );
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn strict_sidecar_skips_and_reports_skipped_shards() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_last_full_now(&layout);

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        boot_options(),
    )
    .expect("boot");
    let check = boot.backend.report().storage_check.expect("storage_check");
    assert_eq!(check.contract_version, 2);
    assert_eq!(check.state, "ok");
    assert_eq!(check.write_gate, "open");
    assert!(!check.main_checked, "严格跳过本轮不检查主库");
    assert_eq!(check.planned_shards, 0);
    assert_eq!(check.skipped_shards, 8);
    assert_eq!(check.checked_shards, 0);
    assert!(!check.full_coverage, "严格跳过不是全量覆盖");

    let storage = boot.storage.take().expect("storage");
    assert_eq!(storage.clean_start_count(), 1, "严格干净启动 +1");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn missing_sidecar_full_fallback_reports_full_coverage() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate");

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        boot_options(),
    )
    .expect("boot");
    let check = boot.backend.report().storage_check.expect("storage_check");
    assert_eq!(check.contract_version, 2);
    assert_eq!(check.state, "ok");
    assert_eq!(check.write_gate, "open");
    assert!(check.main_checked);
    assert_eq!(check.planned_shards, 8);
    assert_eq!(check.skipped_shards, 0);
    assert_eq!(check.checked_shards, 8);
    assert!(check.full_coverage, "全量回退启动检查 = 全量覆盖");
    let diagnostics = boot.storage_check.diagnostics();
    assert_eq!(diagnostics.shards.len(), 8);

    let storage = boot.storage.take().expect("storage");
    assert!(
        storage.background_check_completed(),
        "全量回退启动检查成功后置全量覆盖标志"
    );
    runtime.block_on(storage.shutdown()).expect("shutdown");
}
