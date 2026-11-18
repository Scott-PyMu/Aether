//! M10-14 / ADR-040 D-040-1：启动检查计划（选择性 / 全量回退）单测。
//!
//! 覆盖：干净 sidecar 严格跳过；仅主库变脏；dirty 1/2/4/8；8 片全脏
//! `full_fallback=true`；sidecar 缺字段/`format_version`/`background_check_completed=false`/
//! WAL≠0/schema/分片集合/主库缺指纹 → 全量。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    plan_startup_checks, read_clean_shutdown, read_layout, write_clean_shutdown, StoreRuntime,
    WriteQueueConfig,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// 初始化 generation，执行一次干净关闭并写 sidecar（`background_check_completed=true`）。
fn clean_layout(dir: &Path) -> (aether_store::StorageLayout, tokio::runtime::Runtime) {
    let layout = initialize_layout(dir, "0.1.0", 1).expect("init");
    let runtime = runtime();
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");
    (layout, runtime)
}

fn dirty_main_in_sidecar(layout: &aether_store::StorageLayout) {
    let mut sidecar = read_clean_shutdown(layout).expect("sidecar");
    sidecar.main.file_size = sidecar.main.file_size.saturating_add(1);
    write_clean_shutdown(layout, &sidecar).expect("write sidecar");
}

fn dirty_shards(layout: &aether_store::StorageLayout, ids: &[u8]) {
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
    write_clean_shutdown(layout, &sidecar).expect("write sidecar");
}

fn write_sidecar_json(layout: &aether_store::StorageLayout, value: &serde_json::Value) {
    std::fs::write(
        layout.sidecar_path(),
        serde_json::to_vec_pretty(value).expect("json"),
    )
    .expect("write sidecar");
}

#[test]
fn strict_sidecar_skips_startup_check() {
    let dir = tempfile::tempdir().expect("temp");
    let (layout, _runtime) = clean_layout(dir.path());
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.strict_ok, "全部指纹匹配必须严格跳过：{plan:?}");
    assert!(!plan.main);
    assert!(plan.shards.is_empty());
    assert!(!plan.full_fallback);
    assert_eq!(plan.reason, "sidecar_strict");
    assert_eq!(plan.skipped_shards(8), 8);
    assert!(!plan.full_coverage());
}

#[test]
fn main_only_dirty_checks_main_and_zero_shards() {
    let dir = tempfile::tempdir().expect("temp");
    let (layout, _runtime) = clean_layout(dir.path());
    dirty_main_in_sidecar(&layout);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(!plan.strict_ok);
    assert!(plan.main, "仅主库变脏也必须检查主库");
    assert!(plan.shards.is_empty(), "仅主库变化时不得扩大检查分片");
    assert!(!plan.full_fallback);
    assert_eq!(plan.reason, "main_fingerprint_mismatch");
    assert_eq!(plan.planned_shards(), 0);
    assert_eq!(plan.skipped_shards(8), 8);
}

#[test]
fn dirty_1_2_4_shards_are_selective() {
    for ids in [vec![3u8], vec![0u8, 5], vec![1u8, 2, 6, 7]] {
        let dir = tempfile::tempdir().expect("temp");
        let (layout, _runtime) = clean_layout(dir.path());
        dirty_shards(&layout, &ids);
        let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
        assert!(!plan.strict_ok);
        assert!(plan.main, "任一分片变脏 → 主库必查");
        assert_eq!(plan.shards, ids, "只检查指纹变化分片");
        assert!(!plan.full_fallback, "不足 8 片不是全量回退");
        assert_eq!(plan.planned_shards(), ids.len());
        assert_eq!(plan.skipped_shards(8), 8 - ids.len());
        assert_eq!(plan.reason, "shard_fingerprint_mismatch");
    }
}

#[test]
fn dirty_all_8_shards_is_full_fallback() {
    let dir = tempfile::tempdir().expect("temp");
    let (layout, _runtime) = clean_layout(dir.path());
    let all: Vec<u8> = (0..8).collect();
    dirty_shards(&layout, &all);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(!plan.strict_ok);
    assert!(plan.main);
    assert_eq!(plan.shards, all);
    assert!(plan.full_fallback, "8 片全变等价全量");
    assert_eq!(plan.planned_shards(), 8);
    assert_eq!(plan.skipped_shards(8), 0);
    assert!(plan.full_coverage());
}

#[test]
fn main_and_shard_fingerprint_mismatch_is_selective_with_main() {
    let dir = tempfile::tempdir().expect("temp");
    let (layout, _runtime) = clean_layout(dir.path());
    dirty_main_in_sidecar(&layout);
    dirty_shards(&layout, &[2]);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.main);
    assert_eq!(plan.shards, vec![2]);
    assert_eq!(plan.reason, "main_and_shard_fingerprint_mismatch");
}

#[test]
fn invalid_sidecar_entries_fall_back_to_full() {
    let dir = tempfile::tempdir().expect("temp");
    let (layout, _runtime) = clean_layout(dir.path());

    let base = read_clean_shutdown(&layout).expect("sidecar");
    let base_json = serde_json::to_value(&base).expect("json");

    // format_version 不符
    let mut value = base_json.clone();
    value["format_version"] = serde_json::json!(99);
    write_sidecar_json(&layout, &value);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "sidecar_format_version_mismatch");

    // format_version 缺失（旧 sidecar）
    let mut value = base_json.clone();
    value
        .as_object_mut()
        .expect("object")
        .remove("format_version");
    write_sidecar_json(&layout, &value);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(
        plan.full_fallback,
        "旧 sidecar 缺 format_version 必须全量回退"
    );

    // background_check_completed=false
    let mut sidecar = base.clone();
    sidecar.background_check_completed = false;
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "background_check_incomplete");

    // sidecar WAL ≠ 0
    let mut sidecar = base.clone();
    sidecar.main.wal_size = 4096;
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "wal_not_checkpointed");

    // schema 变化（sidecar 记录值 vs 期望常量）
    let mut sidecar = base.clone();
    sidecar.main.schema_version = 13;
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "schema_version_mismatch");

    // 分片集合不一致
    let mut sidecar = base.clone();
    sidecar.shards.pop();
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "shard_set_mismatch");

    // 分片重复（集合不是恰好 0..8）
    let mut sidecar = base.clone();
    sidecar.shards[1].shard_id = sidecar.shards[0].shard_id;
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "shard_set_mismatch");

    // 主库缺指纹（JSON 解析失败 → sidecar_missing）
    let mut value = base_json.clone();
    value.as_object_mut().expect("object").remove("main");
    write_sidecar_json(&layout, &value);
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "sidecar_missing");

    // app_version 不符
    let mut sidecar = base;
    sidecar.app_version = "previous-version".to_owned();
    write_clean_shutdown(&layout, &sidecar).expect("write");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(plan.full_fallback);
    assert_eq!(plan.reason, "app_version_mismatch");

    // 恢复有效 sidecar（后续测试/布局可继续打开）
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    write_clean_shutdown(&layout, &sidecar).expect("rewrite");
    assert!(read_layout(dir.path()).is_ok());
}
