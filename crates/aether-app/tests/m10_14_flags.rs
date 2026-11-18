//! M10-14 / ADR-040 D-040-3 §2/§3：`background_check_completed` 与 `clean_start_count` 规则。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, StaticRuntimeSummaries,
};
use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    plan_startup_checks, read_clean_shutdown, write_clean_shutdown, StorageLayout, StoreRuntime,
    WriteQueueConfig,
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
            entry.database.file_size = entry.database.file_size.saturating_add(1);
        }
    }
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn options() -> CoreBootOptions {
    CoreBootOptions {
        scheduler: SchedulerConfig {
            enabled: false,
            background_delay: Duration::from_secs(3600),
            ..SchedulerConfig::default()
        },
        ..CoreBootOptions::default()
    }
}

#[test]
fn selective_startup_does_not_set_full_coverage_flag() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_last_full_now(&layout);
    mark_dirty(&layout, &[0, 7]);

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("boot");
    let storage = boot.storage.take().expect("storage");
    assert!(
        !storage.background_check_completed(),
        "选择性启动通过不得置 background_check_completed"
    );
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert_eq!(sidecar.format_version, 1);
    assert!(!sidecar.background_check_completed);
    assert_eq!(sidecar.clean_start_count, 0, "选择性启动不是严格干净启动");
}

#[test]
fn selective_then_immediate_close_forces_full_fallback_next_start() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_last_full_now(&layout);
    mark_dirty(&layout, &[3]);

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("boot");
    // 后台全量尚未开始/完成即关闭。
    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert!(!sidecar.background_check_completed);

    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback, "选择性后立即关闭 → 下次全量回退");
    assert_eq!(plan.reason, "background_check_incomplete");
    assert_eq!(plan.shards.len(), 8);
}

#[test]
fn full_fallback_sets_flag_and_strict_start_increments_clean_count() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate");

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("boot");
    let storage = boot.storage.take().expect("storage");
    assert!(
        storage.background_check_completed(),
        "全量回退启动检查完成后置位"
    );
    assert_eq!(storage.clean_start_count(), 0, "非严格启动不计数");
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let mut sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert!(sidecar.background_check_completed);
    assert_eq!(sidecar.clean_start_count, 0);

    // 下一次严格干净启动 +1（固定 last_full，避免周期检查同时归零）。
    sidecar.last_full_check_at = Some(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("ms"),
    );
    write_clean_shutdown(&layout, &sidecar).expect("sidecar");
    let second = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("second boot");
    let second_storage = second.storage.take().expect("storage");
    assert_eq!(second_storage.clean_start_count(), 1, "严格干净启动 +1");
    runtime
        .block_on(second_storage.shutdown())
        .expect("second shutdown");
}
