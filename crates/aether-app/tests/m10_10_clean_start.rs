//! M10-10 严格 sidecar 与 `clean_start_count` 重置规则（ADR-039 D-039-2 / P2-3）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    read_clean_shutdown, read_layout, write_clean_shutdown, StoreRuntime, WriteQueueConfig,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn clean_start_count_increments_only_on_strict_sidecar_and_resets_on_mismatch() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let runtime = runtime();

    // 第一次干净启动：写入 sidecar（background_check_completed=true）。
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let mut sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert_eq!(sidecar.clean_start_count, 0);
    sidecar.clean_start_count = 5;
    write_clean_shutdown(&layout, &sidecar).expect("write sidecar");

    // 严格匹配：5 → 6。
    let strict = StoreRuntime::open_generation_deferred(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("strict open");
    assert_eq!(strict.clean_start_count(), 6, "严格 sidecar 必须 +1");
    strict.mark_background_check_completed();
    runtime
        .block_on(strict.shutdown())
        .expect("strict shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar after strict");
    assert_eq!(sidecar.clean_start_count, 6);

    // 非严格（版本变化）：必须重置为 0。
    let mut stale = sidecar.clone();
    stale.app_version = "previous-version".to_owned();
    write_clean_shutdown(&layout, &stale).expect("stale sidecar");
    let reset = StoreRuntime::open_generation_deferred(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("non-strict open");
    assert_eq!(reset.clean_start_count(), 0, "sidecar 不匹配必须清零");
    reset.mark_background_check_completed();
    runtime.block_on(reset.shutdown()).expect("reset shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar after reset");
    assert_eq!(sidecar.clean_start_count, 0);

    // 下次严格启动再从 0 → 1（重置后不沿用旧计数）。
    let mut fresh = sidecar;
    fresh.app_version = env!("CARGO_PKG_VERSION").to_owned();
    write_clean_shutdown(&layout, &fresh).expect("fresh sidecar");
    let again = StoreRuntime::open_generation_deferred(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("strict open again");
    assert_eq!(again.clean_start_count(), 1);
    runtime.block_on(again.shutdown()).expect("shutdown again");
    assert!(read_layout(dir.path()).is_ok());
}
