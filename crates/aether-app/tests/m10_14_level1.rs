//! M10-14 Level 1 回退（ADR-040 §7.1/§7.2）：`STARTUP_CHECK_SELECTIVE_ENABLED=false`。
//!
//! 仅在 `--features startup-check-level1` 构建运行；严格跳过保持不变，
//! 任一指纹不匹配 → 主库 + 全部分片；health v2 字段仍保留。
#![cfg(feature = "startup-check-level1")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    plan_startup_checks, read_clean_shutdown, write_clean_shutdown, STARTUP_CHECK_SELECTIVE_ENABLED,
};

#[test]
fn level1_disables_selective_but_keeps_strict_skip() {
    assert!(!STARTUP_CHECK_SELECTIVE_ENABLED, "Level 1 必须关闭选择性");
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let storage = aether_store::StoreRuntime::open_generation(
        layout.clone(),
        aether_store::WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");

    let strict = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(strict.strict_ok, "严格跳过不受 Level 1 影响");

    let mut sidecar = read_clean_shutdown(&layout).expect("sidecar");
    sidecar.shards[0].database.file_size = sidecar.shards[0].database.file_size.saturating_add(1);
    write_clean_shutdown(&layout, &sidecar).expect("sidecar");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(!plan.strict_ok);
    assert!(plan.main);
    assert_eq!(plan.shards, (0..8).collect::<Vec<u8>>());
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "selective_disabled");
    assert!(Path::new(dir.path()).exists());
}
