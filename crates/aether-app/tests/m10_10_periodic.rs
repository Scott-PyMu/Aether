//! M10-10 周期性完整检查：20 次启动触发后台 `integrity_check` 并重置计数。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_app::core_health::{boot_core_full, StaticRuntimeSummaries};
use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    read_clean_shutdown, read_layout, write_clean_shutdown, StoreRuntime, WriteQueueConfig,
};

#[test]
fn periodic_full_integrity_check_after_20_starts() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
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
    let mut sidecar = read_clean_shutdown(&layout).expect("sidecar");
    sidecar.clean_start_count = 19;
    sidecar.last_full_check_at = None;
    write_clean_shutdown(&layout, &sidecar).expect("sidecar write");

    let boot = boot_core_full(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("boot");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = boot.storage_check.snapshot();
        if snapshot.mode == "background" && snapshot.state == "ok" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "周期完整检查未完成：{snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let storage = boot.storage.take().expect("storage");
    assert!(storage.last_full_check_at().is_some());
    assert_eq!(storage.clean_start_count(), 0);
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar after");
    assert_eq!(sidecar.clean_start_count, 0);
    assert!(sidecar.last_full_check_at.is_some());
    drop(read_layout(dir.path()).expect("layout"));
}
