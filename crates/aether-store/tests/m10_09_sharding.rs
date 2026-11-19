//! M10-09（ADR-039 v1.0）事件分库实施——store 层验收。
//!
//! 覆盖：generation 初始化/路径约束；稳定分片路由；主库 `events=0`；
//! 单写任务按会话路由；读路由；会话删除 tombstone/启动恢复；
//! 整套备份集/恢复 staging；故障注入（分片缺失/哈希不符/越界）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_core::{
    EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, RuntimeId, SessionId,
    EVENT_ENVELOPE_VERSION,
};
use aether_store::layout::{
    assert_invariants, initialize as initialize_layout, open_or_initialize as open_layout,
    read_current, read_existing as read_layout, shard_for, write_current, StorageLayout,
    CLEAN_SHUTDOWN_NAME, SHARD_COUNT,
};
#[cfg(windows)]
use aether_store::replace_file_atomic;
use aether_store::{
    create_backup_set, plan_startup_checks, validate_backup_set, Store, StoreCommand, StoreRuntime,
    WriteQueueConfig,
};
use rusqlite::Connection;

const SESSION_A: &str = "01J8ZQ00000000000000000001";
const SESSION_B: &str = "01J8ZQ00000000000000000002";
const SESSION_C: &str = "01J8ZQ00000000000000000003";

fn seed_session(layout: &StorageLayout, session_id: &str) {
    let store = Store::open(layout.main_db()).expect("主库");
    store
        .execute_write(
            "INSERT OR IGNORE INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .expect("runtime");
    store
        .execute_write(&format!(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at, shard_id) \
             VALUES ('{session_id}', 'mock', 't', 'idle', 1, 1, {})",
            shard_for(session_id)
        ))
        .expect("session");
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

fn count_events(path: &Path, session_id: &str) -> i64 {
    let connection = Connection::open(path).expect("open shard");
    connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .expect("count")
}

fn runtime_for(layout: StorageLayout) -> (StoreRuntime, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio");
    let storage =
        StoreRuntime::open_generation(layout, WriteQueueConfig::default(), runtime.handle())
            .expect("open generation runtime");
    (storage, runtime)
}

#[test]
fn generation_initialize_eight_shards_and_current_pointer() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1_700_000_000_000).expect("init");
    assert_eq!(layout.shard_paths().len(), SHARD_COUNT);
    assert!(layout.main_db().is_file());
    let pointer = read_current(dir.path()).expect("CURRENT");
    assert_eq!(pointer.format_version, 1);
    assert_eq!(pointer.generation, layout.generation());
    let manifest = std::fs::read_to_string(layout.shards_json()).expect("shards.json");
    assert!(!manifest.contains("\"generation\""));
    let main = Connection::open(layout.main_db()).expect("main");
    let auto_vacuum: i64 = main
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .expect("auto_vacuum");
    assert_eq!(auto_vacuum, 2, "新装主库 auto_vacuum=INCREMENTAL");
    for path in layout.shard_paths() {
        let shard = Connection::open(path).expect("shard");
        assert_eq!(
            shard
                .query_row("PRAGMA auto_vacuum", [], |row| row.get::<_, i64>(0))
                .expect("shard auto_vacuum"),
            2
        );
        assert_eq!(
            shard
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .expect("events"),
            0
        );
    }
}

#[test]
fn stable_routing_vectors_are_fixed() {
    let vectors = [
        (SESSION_A, 2u8),
        (SESSION_B, 7),
        (SESSION_C, 4),
        ("01J8ZQ00000000000000000004", 1),
        ("01J8ZQ00000000000000000005", 6),
        ("01J8ZQ00000000000000000006", 3),
        ("01J8ZQ00000000000000000007", 0),
        ("01J8ZQ00000000000000000008", 5),
    ];
    for (session, shard) in vectors {
        assert_eq!(shard_for(session), shard, "{session}");
    }
}

#[test]
fn event_writes_route_to_shard_and_reads_route_back() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    seed_session(&layout, SESSION_B);
    let main_path = layout.main_db().to_path_buf();
    let shard_a = layout
        .shard_path(shard_for(SESSION_A))
        .expect("shard a")
        .to_path_buf();
    let shard_b = layout
        .shard_path(shard_for(SESSION_B))
        .expect("shard b")
        .to_path_buf();
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();
    runtime.block_on(async {
        queue
            .append_events(vec![event(SESSION_A, 1), event(SESSION_A, 2)])
            .await
            .expect("append");
        let session = SessionId::new(SESSION_A).expect("id");
        assert_eq!(reads.event_count(&session).await.expect("count"), 2);
        assert_eq!(reads.max_seq(&session).await.expect("max"), Some(2));
        let page = reads.events_page(&session, None, 50).await.expect("page");
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].seq, 2);
    });
    assert_eq!(count_events(&main_path, SESSION_A), 0, "主库 events 恒空");
    assert_eq!(count_events(&shard_a, SESSION_A), 2);
    assert_eq!(count_events(&shard_b, SESSION_B), 0);
    runtime.block_on(async {
        storage.shutdown().await.expect("shutdown");
    });
    let sidecar = layout.generation_dir().join(CLEAN_SHUTDOWN_NAME);
    assert!(sidecar.is_file(), "干净关闭写 clean_shutdown.json");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("sidecar")).expect("json");
    assert_eq!(value["generation"], layout.generation());
}

#[test]
fn session_delete_tombstone_removes_shard_events_and_is_idempotent() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let main_path = layout.main_db().to_path_buf();
    let shard_path = layout
        .shard_path(shard_for(SESSION_A))
        .expect("shard")
        .to_path_buf();
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    runtime.block_on(async {
        queue
            .append_events(vec![event(SESSION_A, 1)])
            .await
            .expect("append");
        let outcome = queue
            .execute(StoreCommand::DeleteSession {
                id: SessionId::new(SESSION_A).expect("id"),
            })
            .await
            .expect("delete");
        assert!(matches!(
            outcome,
            aether_store::StoreOutcome::SessionDeleted {
                events_retained: 0,
                ..
            }
        ));
        let again = queue
            .execute(StoreCommand::DeleteSession {
                id: SessionId::new(SESSION_A).expect("id"),
            })
            .await
            .expect("retry");
        assert!(matches!(again, aether_store::StoreOutcome::SessionNotFound));
    });
    assert_eq!(count_events(&shard_path, SESSION_A), 0);
    let main = Connection::open(&main_path).expect("main");
    let sessions: i64 = main
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            [SESSION_A],
            |row| row.get(0),
        )
        .expect("sessions");
    assert_eq!(sessions, 0);
    runtime.block_on(async {
        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn startup_recovers_pending_deletion_tombstone() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let shard_path = layout
        .shard_path(shard_for(SESSION_A))
        .expect("shard")
        .to_path_buf();
    let connection = Connection::open(layout.main_db()).expect("main");
    connection
        .execute(
            "UPDATE sessions SET deletion_pending = 1 WHERE id = ?1",
            [SESSION_A],
        )
        .expect("pending");
    drop(connection);
    let shard = Connection::open(&shard_path).expect("shard");
    shard
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES ('01J8ZQ00000000000000PEND1', ?1, NULL, 'mock', 1, 'log', '{}', 1, 1)",
            [SESSION_A],
        )
        .expect("event");
    drop(shard);
    let (storage, runtime) = runtime_for(layout.clone());
    // 写任务启动前已执行 tombstone 收尾（open_generation 读取 pending 并在 writer 启动时删除）。
    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });
    assert_eq!(count_events(&shard_path, SESSION_A), 0);
    let main = Connection::open(storage.path()).expect("main");
    let count: i64 = main
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            [SESSION_A],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 0, "启动恢复完成删除");
    runtime.block_on(async {
        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn startup_invariants_fail_closed() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(dir.path().join("aether.db"), b"legacy-flat").expect("flat");
    assert!(
        open_layout(dir.path(), "0.1.0", 1).is_err(),
        "旧扁平布局拒绝"
    );

    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let connection = Connection::open(layout.main_db()).expect("main");
    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
        )
        .expect("runtime");
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at, shard_id) \
             VALUES (?1, 'mock', 't', 'idle', 1, 1, ?2)",
            rusqlite::params![SESSION_A, i64::from(shard_for(SESSION_A))],
        )
        .expect("session");
    connection
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES ('01J8ZQ00000000000000MAIN1', ?1, NULL, 'mock', 1, 'log', '{}', 1, 1)",
            [SESSION_A],
        )
        .expect("main event");
    assert!(assert_invariants(&layout).is_err(), "主库 events 非空拒绝");

    connection
        .execute("DELETE FROM events", [])
        .expect("clear events");
    connection
        .execute(
            "UPDATE sessions SET shard_id = 99 WHERE id = ?1",
            [SESSION_A],
        )
        .expect("bad shard");
    assert!(assert_invariants(&layout).is_err(), "shard_id 越界拒绝");

    connection
        .execute(
            "UPDATE sessions SET shard_id = ?2 WHERE id = ?1",
            rusqlite::params![SESSION_A, i64::from(shard_for(SESSION_A))],
        )
        .expect("fix shard");
    connection
        .execute(
            "UPDATE sessions SET deletion_pending = 2 WHERE id = ?1",
            [SESSION_A],
        )
        .expect_err("CHECK 拒绝 deletion_pending=2");
}

#[test]
fn current_and_shards_integrity_are_fail_closed() {
    let dir = tempfile::tempdir().expect("temp");
    initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    // shards.json 哈希不匹配。
    let mut pointer = read_current(dir.path()).expect("CURRENT");
    pointer.shards_sha256 = "deadbeef".to_owned();
    write_current(dir.path(), &pointer).expect("write");
    assert!(read_layout(dir.path()).is_err(), "shards_sha256 不符拒绝");

    // format_version 不支持。
    let dir3 = tempfile::tempdir().expect("temp3");
    initialize_layout(dir3.path(), "0.1.0", 1).expect("init3");
    let mut pointer = read_current(dir3.path()).expect("CURRENT3");
    pointer.format_version = 2;
    write_current(dir3.path(), &pointer).expect("write3");
    assert!(
        read_layout(dir3.path()).is_err(),
        "format_version 不支持拒绝"
    );

    // 删除一个分片 → 拒绝打开。
    let dir2 = tempfile::tempdir().expect("temp2");
    let layout2 = initialize_layout(dir2.path(), "0.1.0", 1).expect("init2");
    let pointer = read_current(dir2.path()).expect("CURRENT2");
    std::fs::remove_file(layout2.shard_path(0).expect("shard0")).expect("remove shard");
    assert!(read_layout(dir2.path()).is_err(), "分片缺失拒绝");
    assert_eq!(pointer.generation, layout2.generation());
}

#[test]
fn write_p95_and_events_page_p95_are_within_target() {
    // 共享 CI runner（尤其 macOS debug）的调度抖动不适合作为 50ms 性能门禁；
    // 标准 600s、5.22GiB 性能证据由 `m10_09_bench`/docs metrics 负责。
    let strict_perf = std::env::var_os("CI").is_none();
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let mut session_ids = Vec::new();
    for index in 0..10 {
        let id = format!("01J8ZQ0000000000000000W{index:02}");
        seed_session(&layout, &id);
        session_ids.push(id);
    }
    let (storage, runtime) = runtime_for(layout);
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();
    let mut samples = Vec::new();
    runtime.block_on(async {
        let mut tasks = tokio::task::JoinSet::new();
        for (index, session_id) in session_ids.iter().cloned().enumerate() {
            let queue = queue.clone();
            tasks.spawn(async move {
                let mut samples = Vec::new();
                for seq in 1..=100u64 {
                    let mut event = event(&session_id, seq);
                    event.id = EventId::new(format!("01J8ZQ{index:02}{seq:018}")).expect("id");
                    let started = std::time::Instant::now();
                    queue.append_events(vec![event]).await.expect("append");
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                samples
            });
        }
        while let Some(joined) = tasks.join_next().await {
            samples.extend(joined.expect("task"));
        }
        // 读路由 P95（同一会话补读 100 次）。
        let session = SessionId::new(&session_ids[0]).expect("id");
        let mut read_samples = Vec::new();
        for _ in 0..100 {
            let started = std::time::Instant::now();
            let page = reads.events_page(&session, None, 500).await.expect("page");
            assert_eq!(page.len(), 100);
            read_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        read_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95_read = read_samples[(read_samples.len() * 95 / 100).min(read_samples.len() - 1)];
        println!("M10-09 events_page_p95_ms={p95_read:.3}");
        if strict_perf {
            assert!(p95_read < 150.0, "events_page P95 {p95_read:.3}ms ≥150ms");
        } else {
            eprintln!("SKIP CI: events_page P95 仅记录（{p95_read:.3}ms）");
        }
        storage.shutdown().await.expect("shutdown");
    });
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    println!(
        "M10-09 write_p95_ms={p95:.3} samples={} max={:.3}",
        samples.len(),
        samples.last().copied().unwrap_or(0.0)
    );
    if strict_perf {
        assert!(p95 < 50.0, "写 P95 {p95:.3}ms ≥50ms");
    } else {
        eprintln!("SKIP CI: 写 P95 仅记录（{p95:.3}ms）");
    }
}

#[test]
fn old_generations_keep_latest_and_skip_current() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let generations = dir.path().join("generations");
    for name in [".pre-restore-1", ".pre-restore-2", ".failed-1"] {
        let path = generations.join(name);
        std::fs::create_dir_all(&path).expect("dir");
        std::fs::write(path.join("marker"), b"x").expect("file");
    }
    let removed = aether_store::cleanup_generations(dir.path(), 2);
    let names: Vec<String> = std::fs::read_dir(&generations)
        .expect("read")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&layout.generation().to_owned()),
        "当前 generation 不删除"
    );
    assert!(
        names.contains(&".pre-restore-2".to_owned()),
        "最近 1 份 .pre-* 保留"
    );
    assert!(
        names.contains(&".failed-1".to_owned()),
        "最近 1 份 .failed-* 保留"
    );
    assert!(
        !names.contains(&".pre-restore-1".to_owned()),
        "更早 .pre-* 清理"
    );
    assert_eq!(removed.len(), 1);
}

#[test]
fn maintenance_pause_blocks_writes_until_resume() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    runtime.block_on(async {
        let gate = queue.pause_and_drain().await.expect("pause");
        let writer_queue = queue.clone();
        let append =
            tokio::spawn(
                async move { writer_queue.append_events(vec![event(SESSION_A, 1)]).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!append.is_finished(), "维护态必须阻塞新写入");
        drop(gate);
        append.await.expect("join").expect("append after resume");
        storage.shutdown().await.expect("shutdown");
    });
    let shard = layout
        .shard_path(shard_for(SESSION_A))
        .expect("shard")
        .to_path_buf();
    assert_eq!(count_events(&shard, SESSION_A), 1);
}

#[cfg(windows)]
#[test]
fn current_replacement_replaces_existing_pointer() {
    let dir = tempfile::tempdir().expect("temp");
    let destination = dir.path().join("CURRENT");
    let temporary = dir.path().join("CURRENT.tmp");
    std::fs::write(&destination, b"old").expect("old");
    std::fs::write(&temporary, b"new").expect("tmp");
    replace_file_atomic(&temporary, &destination).expect("MoveFileExW 替换既有 CURRENT");
    assert_eq!(std::fs::read(&destination).expect("read"), b"new");
    assert!(!temporary.exists(), "临时指针必须被替换消费");
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
#[test]
fn current_replacement_failure_keeps_old_pointer() {
    let dir = tempfile::tempdir().expect("temp");
    let destination = dir.path().join("CURRENT");
    let temporary = dir.path().join("CURRENT.tmp");
    std::fs::write(&destination, b"old").expect("old");
    std::fs::write(&temporary, b"new").expect("tmp");
    let mut permissions = std::fs::metadata(&destination)
        .expect("metadata")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&destination, permissions).expect("readonly");
    let result = replace_file_atomic(&temporary, &destination);
    let mut permissions = std::fs::metadata(&destination)
        .expect("metadata2")
        .permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&destination, permissions).expect("writable");
    assert!(result.is_err(), "只读 CURRENT 的替换必须失败");
    assert_eq!(std::fs::read(&destination).expect("old content"), b"old");
}

#[test]
fn mismatch_check_plan_is_selective_for_changed_shard() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let (storage, runtime) = runtime_for(layout.clone());
    runtime.block_on(async {
        storage
            .queue()
            .append_events(vec![event(SESSION_A, 1)])
            .await
            .expect("append");
        storage.mark_background_check_completed();
        storage.shutdown().await.expect("shutdown");
    });
    let strict = plan_startup_checks(&layout, "0.1.0");
    assert!(strict.strict_ok, "正常关闭后应严格跳过");

    let shard_id = shard_for(SESSION_A);
    let shard = layout.shard_path(shard_id).expect("shard");
    let connection = Connection::open(shard).expect("open shard");
    connection
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES ('01J8ZQ0000000000000000DIRT', ?1, NULL, 'mock', 2, 'log', '{\"level\":\"info\"}', 2, 1)",
            [SESSION_A],
        )
        .expect("dirty shard");
    connection.close().expect("close");
    let plan = plan_startup_checks(&layout, "0.1.0");
    assert!(!plan.strict_ok);
    // ADR-040 D-040-1：任一指纹变化 → 主库必查；分片仅检查指纹变化项。
    assert!(plan.main, "出现脏分片时必须检查主库");
    assert_eq!(
        plan.shards,
        vec![shard_id],
        "只有指纹变化的分片进入启动计划"
    );
    assert!(!plan.full_fallback, "单个脏分片不是全量回退");
    assert_eq!(plan.skipped_shards(8), 7);
    assert!(!plan.full_coverage());
}

#[test]
fn periodic_full_check_due_after_20_starts_and_resets() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    let (storage, runtime) = runtime_for(layout.clone());
    storage.mark_background_check_completed();
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
    let mut sidecar = aether_store::read_clean_shutdown(&layout).expect("sidecar");
    sidecar.clean_start_count = 19;
    sidecar.last_full_check_at = None;
    aether_store::write_clean_shutdown(&layout, &sidecar).expect("sidecar");

    let (storage, runtime) = runtime_for(layout.clone());
    assert!(storage.full_check_due(1_794_800_000_000));
    storage.mark_full_check_completed(1_794_800_000_000);
    assert!(!storage.full_check_due(1_794_800_000_000));
    assert_eq!(storage.clean_start_count(), 0);
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
    let sidecar = aether_store::read_clean_shutdown(&layout).expect("sidecar after");
    assert_eq!(sidecar.clean_start_count, 0);
    assert_eq!(sidecar.last_full_check_at, Some(1_794_800_000_000));
}

#[test]
fn backup_under_write_pressure_drains_and_preserves_events() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    let target = dir.path().join("backups");
    runtime.block_on(async {
        let writer = tokio::spawn(async move {
            for seq in 1..=500u64 {
                queue
                    .append_events(vec![event(SESSION_A, seq)])
                    .await
                    .expect("write");
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let gate = storage.queue().pause_and_drain().await.expect("pause");
        assert_eq!(storage.queue().depth(), 0, "drain 后队列深度必须为 0");
        let outcome = create_backup_set(
            &layout,
            &target,
            "01J8ZQ0000000000000000BK01",
            "manual",
            1_794_800_000_000,
            "0.1.0",
            false,
            "none",
        )
        .expect("backup under write pressure");
        assert_eq!(outcome.manifest.shard_count, 8);
        validate_backup_set(&target.join("01J8ZQ0000000000000000BK01")).expect("validate");
        drop(gate);
        writer.await.expect("writer");
        storage.shutdown().await.expect("shutdown");
    });
    let shard = layout
        .shard_path(shard_for(SESSION_A))
        .expect("shard")
        .to_path_buf();
    assert_eq!(count_events(&shard, SESSION_A), 500);
}

#[test]
fn backup_set_roundtrip_and_external_path_rules() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    runtime.block_on(async {
        queue
            .append_events(vec![event(SESSION_A, 1)])
            .await
            .expect("append");
        storage.shutdown().await.expect("shutdown");
    });
    let target = dir.path().join("backups");
    let outcome = aether_store::create_backup_set(
        &layout,
        &target,
        "01J8ZQ0000000000000000BK01",
        "manual",
        1,
        "0.1.0",
        false,
        "none",
    )
    .expect("backup set");
    assert_eq!(outcome.manifest.shard_count, 8);
    assert_eq!(outcome.record.format.as_deref(), Some("sharded_set"));
    let validated = aether_store::validate_backup_set(&outcome.record.path).expect("validate");
    assert_eq!(validated.manifest.backup_id, outcome.manifest.backup_id);
    // 篡改 manifest.sha256 → 拒绝。
    std::fs::write(outcome.record.path.join("manifest.sha256"), b"deadbeef").expect("tamper");
    assert!(aether_store::validate_backup_set(&outcome.record.path).is_err());
    // 旧 `.db` 单文件拒绝。
    let legacy = dir.path().join("legacy.db");
    std::fs::write(&legacy, b"old").expect("legacy");
    assert!(aether_store::validate_backup_set(&legacy).is_err());
}

#[test]
fn restore_staging_generation_switches_atomically() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1).expect("init");
    seed_session(&layout, SESSION_A);
    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    runtime.block_on(async {
        queue
            .append_events(vec![event(SESSION_A, 1)])
            .await
            .expect("append");
        storage.shutdown().await.expect("shutdown");
    });
    let target = dir.path().join("backups");
    let outcome = aether_store::create_backup_set(
        &layout,
        &target,
        "01J8ZQ0000000000000000BK02",
        "manual",
        1,
        "0.1.0",
        false,
        "none",
    )
    .expect("backup set");
    let validated = aether_store::validate_backup_set(&outcome.record.path).expect("validate");
    let new_generation = aether_store::new_generation_id();
    let journal =
        aether_store::stage_restore(dir.path(), &validated, &new_generation, 2).expect("stage");
    aether_store::write_generation_journal(dir.path(), &journal).expect("journal");
    aether_store::verify_staged_generation(dir.path(), "restore", &new_generation).expect("verify");
    let pointer =
        aether_store::commit_staged_generation(dir.path(), "restore", &journal, 2).expect("commit");
    assert_eq!(pointer.generation, new_generation);
    assert_eq!(
        read_current(dir.path()).expect("current").generation,
        new_generation
    );
    let restored = read_layout(dir.path()).expect("layout");
    assert_invariants(&restored).expect("invariants");
    let shard_path = restored
        .shard_path(shard_for(SESSION_A))
        .expect("shard")
        .to_path_buf();
    assert_eq!(count_events(&shard_path, SESSION_A), 1);
    // 旧 generation 保留为 `.pre-restore-*`。
    let entries: Vec<String> = std::fs::read_dir(dir.path().join("generations"))
        .expect("generations")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.iter().any(|name| name.starts_with(".pre-restore-")));
}
