//! M10-10 故障注入：分片页级损坏 → 启动检查失败、写门保持 checking、S-08 映射依据。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use aether_app::core_health::{boot_core_full, StaticRuntimeSummaries};
use aether_core::{
    EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, RuntimeId, SessionId,
    EVENT_ENVELOPE_VERSION,
};
use aether_store::layout::initialize as initialize_layout;

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
fn corrupted_shard_page_fails_startup_check_and_keeps_write_gate_closed() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let shard = layout.shard_path(0).expect("shard 0").to_path_buf();
    let page_size = 4096u64;
    let size = std::fs::metadata(&shard).expect("metadata").len();
    let store = aether_store::Store::open(&shard).expect("open shard");
    let events_root: i64 = store
        .connection()
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE name = 'events'",
            [],
            |row| row.get(0),
        )
        .expect("events rootpage");
    drop(store);
    let events_page = u64::try_from(events_root).expect("rootpage");
    assert!(
        events_page > 2 && size >= page_size * events_page,
        "events 根页不可用（page={events_page}, size={size}）"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&shard)
        .expect("open shard");
    // 破坏 `events` b-tree 根页页头（不触碰 schema_migrations；启动预检可通过，
    // `quick_check` 必须失败并进入写门/S-08 路径）。
    file.seek(SeekFrom::Start(page_size * (events_page - 1)))
        .expect("seek");
    file.write_all(&[0u8; 8]).expect("corrupt page header");
    file.sync_all().expect("sync");
    let mut header = [0u8; 100];
    file.seek(SeekFrom::Start(0)).expect("seek header");
    file.read_exact(&mut header).expect("read header");
    assert_eq!(&header[..16], b"SQLite format 3\0", "文件头必须保持有效");
    drop(file);
    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate sidecar");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let boot = boot_core_full(
        Path::new(&dir.path()),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("boot core");
    let check = boot.storage_check.snapshot();
    assert_eq!(check.state, "failed", "页级损坏必须使启动检查失败");
    assert_eq!(check.write_gate, "checking", "失败后写门保持关闭");
    assert!(
        check
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("quick_check"),
        "错误必须包含 quick_check：{:?}",
        check.error
    );
    let diagnostics = boot.storage_check.diagnostics();
    assert!(diagnostics.failure_count >= 1, "失败数必须 >=1");
    assert!(
        diagnostics
            .shards
            .iter()
            .any(|entry| entry.shard_id == 0 && !entry.ok),
        "必须记录分片 0 失败样本"
    );

    // 写门关闭：事件写入必须被拒绝（读仍可用）。
    let write = boot.write.clone();
    let error = runtime
        .block_on(write.append_events(vec![event("01J8ZQ00000000000000000001", 1)]))
        .expect_err("写门关闭时必须拒绝写入");
    assert!(
        matches!(error, aether_store::StoreError::StorageBackpressure { .. }),
        "期望 StorageBackpressure，实际 {error:?}"
    );
    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });
    let report = boot.backend.report();
    assert_eq!(report.storage_state, "persist_degraded");
    let storage = boot.storage.take().expect("storage");
    let _ = runtime.block_on(storage.shutdown());
}
