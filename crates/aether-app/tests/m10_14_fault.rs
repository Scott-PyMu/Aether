//! M10-14 / ADR-040 §3.3 #1/#2/#9：干净指纹 + 页级损坏（选择性跳过 → 后台全量发现/停止写入）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, StaticRuntimeSummaries,
};
use aether_core::{
    EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, RuntimeId, SessionId,
    EVENT_ENVELOPE_VERSION,
};
use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    database_fingerprint, plan_startup_checks, read_clean_shutdown, write_clean_shutdown,
    StorageLayout, Store, StoreRuntime, WriteQueueConfig,
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
    sidecar.last_full_check_at = Some(now_ms());
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

fn options() -> CoreBootOptions {
    CoreBootOptions {
        scheduler: SchedulerConfig {
            enabled: false,
            background_delay: Duration::ZERO,
            ..SchedulerConfig::default()
        },
        ..CoreBootOptions::default()
    }
}

/// 破坏 `table` 的 b-tree 根页内容（保持文件头/size/mtime 与 sidecar 一致）。
fn corrupt_table_page(path: &Path, table: &str, modified_at_ms: i64) {
    let store = Store::open(path).expect("open db");
    let root: i64 = store
        .connection()
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE name = ?1",
            [table],
            |row| row.get(0),
        )
        .expect("rootpage");
    drop(store);
    let page_size = 4096u64;
    let root = u64::try_from(root).expect("rootpage");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open raw");
    file.seek(SeekFrom::Start(page_size * (root - 1)))
        .expect("seek page");
    file.write_all(&[0u8; 8]).expect("corrupt page");
    file.sync_all().expect("sync");
    let modified =
        UNIX_EPOCH + Duration::from_millis(u64::try_from(modified_at_ms).expect("mtime"));
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore mtime");
    drop(file);
    let _ = std::fs::remove_file(aether_store::sqlite_sidecar(path, "-wal"));
    let _ = std::fs::remove_file(aether_store::sqlite_sidecar(path, "-shm"));
}

fn event(session_id: &str, seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(format!("01J8ZQ{seq:020}")).expect("event id"),
        session_id: SessionId::new(session_id).expect("session id"),
        run_id: None,
        runtime_id: RuntimeId::new("mock").expect("runtime id"),
        seq,
        ts: i64::try_from(seq).unwrap_or(0),
        payload: EventPayload::Log(LogPayload {
            level: LogLevel::Info,
            message: format!("e{seq}"),
        }),
    }
}

#[test]
fn clean_fingerprint_shard_corruption_is_found_by_background_full_check() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_last_full_now(&layout);
    let shard = layout.shard_path(0).expect("shard 0").to_path_buf();
    let mut sidecar = read_clean_shutdown(&layout).expect("sidecar");
    let expected = sidecar
        .shards
        .iter()
        .find(|entry| entry.shard_id == 0)
        .expect("shard fingerprint")
        .database
        .clone();
    corrupt_table_page(&shard, "events", expected.modified_at_ms);
    // 页内容损坏不改变 size/header；mtime 已恢复 → 启动计划严格跳过该分片。
    let current = database_fingerprint(&shard, expected.last_seq).expect("fingerprint");
    if let Some(entry) = sidecar.shards.iter_mut().find(|entry| entry.shard_id == 0) {
        entry.database = current;
    }
    write_clean_shutdown(&layout, &sidecar).expect("sidecar");
    let plan = plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"));
    assert!(
        plan.strict_ok,
        "干净指纹 + 页级损坏必须进入严格跳过路径：{plan:?}"
    );

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("boot");
    let startup = boot.storage_check.snapshot();
    assert_eq!(startup.state, "ok");
    assert!(!startup.main_checked, "严格跳过启动不检查主库");

    // 后台全量 quick_check 必须发现页级损坏。
    boot.storage_check.request_start();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = boot.storage_check.snapshot();
        if snapshot.state == "failed" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "后台全量未发现损坏");
        std::thread::sleep(Duration::from_millis(20));
    }
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.mode, "background");
    assert!(
        snapshot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("quick_check"),
        "错误必须来自 quick_check：{:?}",
        snapshot.error
    );
    let report = boot.backend.report();
    assert_eq!(report.storage_state, "persist_degraded");
    let write = boot.write.clone();
    let error = runtime
        .block_on(write.append_events(vec![event("01J8ZQ00000000000000000001", 1)]))
        .expect_err("后台检查失败后写门必须关闭");
    assert!(
        matches!(error, aether_store::StoreError::StorageBackpressure { .. }),
        "期望 StorageBackpressure，实际 {error:?}"
    );
    let storage = boot.storage.take().expect("storage");
    let _ = runtime.block_on(storage.shutdown());
}

#[test]
fn clean_fingerprint_main_corruption_is_found_by_full_fallback() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate sidecar");
    let sidecar = read_clean_shutdown(&layout);
    assert!(sidecar.is_none());
    let main_mtime = i64::try_from(
        std::fs::metadata(layout.main_db())
            .expect("metadata")
            .modified()
            .expect("mtime")
            .duration_since(UNIX_EPOCH)
            .expect("epoch")
            .as_millis(),
    )
    .expect("ms");
    corrupt_table_page(layout.main_db(), "messages", main_mtime);

    let boot = boot_core_full_with_options(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        options(),
    )
    .expect("boot");
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.state, "failed");
    assert_eq!(snapshot.write_gate, "checking");
    assert!(snapshot.full_coverage, "无 sidecar → 全量回退计划");
    assert!(
        snapshot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("quick_check"),
        "主库损坏必须由 quick_check 发现：{:?}",
        snapshot.error
    );
    let storage = boot.storage.take().expect("storage");
    let _ = runtime.block_on(storage.shutdown());
}
