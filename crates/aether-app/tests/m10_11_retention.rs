//! M10-11（ADR-039 v1.0 D-039-4）保留清理与文件收缩——app 层验收。
//!
//! 覆盖：
//! - 产品 `RetentionTask` 路径：`(type, ts)` 选批、归档先行、失败跳过、删除事务档位；
//! - 最终清理集合不变、`audit_log` 不清理、补读契约不变；
//! - live/物理分开报告；维护态手动 VACUUM 空间预检、停写窗口、告警；
//! - 维护态进行中清理延后（不阻塞维护操作）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_app::backup_control::SpaceProbe;
use aether_app::ipc::MaintenanceCell;
use aether_app::retention::{
    RetentionDeps, RetentionOutcome, RetentionTask, RetentionThresholds,
    RETENTION_DELETE_BATCH_DEFAULT, RETENTION_DELETE_BATCH_TIERS,
    RETENTION_INCREMENTAL_VACUUM_PAGES,
};
use aether_control::{ManualClock, SharedClock};
use aether_core::SessionId;
use aether_store::layout::{initialize as initialize_layout, shard_for, StorageLayout};
use aether_store::{
    generation_live_bytes, ReadPool, Store, StoreCommand, StoreError, StoreRuntime,
    WriteQueueConfig,
};

const SESSION_SHARD0: &str = "01J8ZQ00000000000000000007";
const NOW_MS: i64 = 1_800_000_000_000;
const OLD_TS: i64 = 1_700_000_000_000;
const IN_WINDOW_TS: i64 = 1_799_000_000_000;

struct FixedSpaceProbe {
    available: u64,
}

impl SpaceProbe for FixedSpaceProbe {
    fn ensure_writable(&self, _dir: &Path) -> Result<(), String> {
        Ok(())
    }

    fn available_bytes(&self, _dir: &Path) -> Result<u64, String> {
        Ok(self.available)
    }
}

fn payload_for(event_type: &str, index: usize) -> String {
    match event_type {
        "log" => format!(r#"{{"level":"info","message":"retention-{index}"}}"#),
        "usage" => format!(
            r#"{{"tokens":{{"input_tokens":{index},"output_tokens":1,"total_tokens":{}}}}}"#,
            index + 1
        ),
        "message.delta" => {
            format!(r#"{{"message_id":"01J000000000000000000000M1","text":"chunk-{index}"}}"#)
        }
        "run.started" => r#"{"run_id":"01J000000000000000000000R1"}"#.to_owned(),
        _ => format!(r#"{{"index":{index}}}"#),
    }
}

fn sql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn insert_events(
    path: &Path,
    session_id: &str,
    start_seq: u64,
    rows: usize,
    event_type: &str,
    ts: i64,
) {
    let store = Store::open(path).expect("open fixture db");
    let indexes: Vec<usize> = (0..rows).collect();
    for chunk in indexes.chunks(500) {
        let mut sql = String::from(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES ",
        );
        for (position, index) in chunk.iter().enumerate() {
            if position > 0 {
                sql.push(',');
            }
            let seq = start_seq.saturating_add(u64::try_from(*index).unwrap_or(0));
            sql.push_str(&format!(
                "({}, {}, NULL, 'mock', {seq}, {}, {}, {ts}, 1)",
                sql_quote(&format!("01J8ZQ{seq:020}")),
                sql_quote(session_id),
                sql_quote(event_type),
                sql_quote(&payload_for(event_type, *index))
            ));
        }
        store.execute_write(&sql).expect("insert fixture events");
    }
}

fn insert_audit(path: &Path) {
    let store = Store::open(path).expect("open main db");
    store
        .execute_write(
            "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, \
             result, ts) VALUES ('01JAUDIT000000000000000001', NULL, NULL, 'system', \
             'retention.test', NULL, 'must-survive-retention', 'success', 1)",
        )
        .expect("insert audit fixture");
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

fn retention_task(
    storage: &StoreRuntime,
    data_dir: &Path,
    maintenance: Arc<MaintenanceCell>,
    space: Arc<dyn SpaceProbe>,
) -> RetentionTask {
    let clock: SharedClock = Arc::new(ManualClock::new(NOW_MS));
    RetentionTask::new(RetentionDeps {
        data_dir: data_dir.to_path_buf(),
        reads: storage.reads().clone(),
        write: storage.queue().clone(),
        clock,
        thresholds: RetentionThresholds::default(),
        maintenance: Some(maintenance),
        space,
    })
}

fn enable_retention(storage: &StoreRuntime, runtime: &tokio::runtime::Runtime) {
    runtime.block_on(async {
        for (key, value) in [
            ("events.retention.enabled", "true"),
            ("events.retention.days", "30"),
        ] {
            storage
                .queue()
                .execute(StoreCommand::UpsertSetting {
                    key: key.to_owned(),
                    value: value.to_owned(),
                    updated_at: NOW_MS,
                })
                .await
                .expect("retention setting");
        }
    });
}

fn count(reads: &ReadPool, runtime: &tokio::runtime::Runtime, sql: &str) -> i64 {
    let reads = reads.clone();
    let sql = sql.to_owned();
    runtime
        .block_on(reads.with_connection(move |connection| {
            connection
                .query_row(&sql, (), |row| row.get::<_, i64>(0))
                .map_err(StoreError::from)
        }))
        .expect("count")
}

fn shard_count(
    reads: &ReadPool,
    runtime: &tokio::runtime::Runtime,
    shard_id: u8,
    sql: &str,
) -> i64 {
    let reads = reads.clone();
    let sql = sql.to_owned();
    runtime
        .block_on(reads.with_shard_connection(shard_id, move |connection| {
            connection
                .query_row(&sql, (), |row| row.get::<_, i64>(0))
                .map_err(StoreError::from)
        }))
        .expect("shard count")
}

#[test]
fn cleanup_uses_product_path_and_reports_live_physical_separately() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
    let shard_id = shard_for(SESSION_SHARD0);
    assert_eq!(shard_id, 0);
    let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
    // 25 条超窗白名单（三类型）+ 5 条控制事件 + 5 条窗口内白名单。
    insert_events(&shard_path, SESSION_SHARD0, 1, 10, "log", OLD_TS);
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        11,
        8,
        "message.delta",
        OLD_TS + 1,
    );
    insert_events(&shard_path, SESSION_SHARD0, 19, 7, "usage", OLD_TS + 2);
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        26,
        5,
        "run.started",
        OLD_TS + 3,
    );
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        31,
        5,
        "message.delta",
        IN_WINDOW_TS,
    );
    insert_audit(layout.main_db());

    let (storage, runtime) = runtime_for(layout.clone());
    enable_retention(&storage, &runtime);
    let maintenance = Arc::new(MaintenanceCell::new());
    let task = retention_task(
        &storage,
        dir.path(),
        Arc::clone(&maintenance),
        Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    );
    let report = runtime.block_on(task.run_once());
    match &report.outcome {
        RetentionOutcome::Cleaned { rows, .. } => {
            assert_eq!(*rows, 25, "产品路径必须清理全部超窗白名单行")
        }
        other => panic!("期望 Cleaned，实际 {other:?}"),
    }

    // 最终集合不变：控制事件 + 窗口内白名单保留。
    assert_eq!(
        shard_count(
            storage.reads(),
            &runtime,
            shard_id,
            "SELECT COUNT(*) FROM events"
        ),
        10
    );
    assert_eq!(
        shard_count(
            storage.reads(),
            &runtime,
            shard_id,
            "SELECT COUNT(*) FROM events WHERE type IN ('message.delta','usage','log') \
             AND ts < 1797000000000"
        ),
        0
    );
    // audit_log 不清理。
    assert_eq!(
        count(
            storage.reads(),
            &runtime,
            "SELECT COUNT(*) FROM audit_log WHERE detail = 'must-survive-retention'"
        ),
        1
    );

    // 补读契约不变：seq 缺口允许，剩余 10 条按 seq 升序。
    let session = SessionId::new(SESSION_SHARD0).expect("session id");
    let page = runtime
        .block_on(storage.reads().events_page(&session, None, 100))
        .expect("events_page");
    assert_eq!(page.len(), 10);
    let seqs: Vec<u64> = page.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![26, 27, 28, 29, 30, 31, 32, 33, 34, 35]);
    assert_eq!(
        page.first().map(|event| event.seq),
        Some(26),
        "缺口从 1 跳到 26"
    );

    let status = runtime.block_on(task.status()).expect("status");
    let capacity = status["capacity"].as_object().expect("capacity");
    let live = capacity["live_bytes"].as_u64().expect("live_bytes");
    let physical = capacity["physical_bytes"].as_u64().expect("physical_bytes");
    assert!(live > 0, "live_bytes 必须报告：{status}");
    assert_eq!(
        capacity["total_bytes"].as_u64(),
        Some(physical),
        "total_bytes 保持物理 db+wal 口径"
    );
    assert_eq!(
        capacity["physical_level"].as_str(),
        Some("ok"),
        "小库物理档位应为 ok"
    );
    assert_eq!(
        status["delete_batch"]["tier"].as_u64(),
        Some(RETENTION_DELETE_BATCH_DEFAULT as u64)
    );
    assert_eq!(
        status["delete_batch"]["tiers"]
            .as_array()
            .map(|values| values.len()),
        Some(RETENTION_DELETE_BATCH_TIERS.len())
    );
    assert_eq!(
        status["vacuum"]["incremental_pages"].as_u64(),
        Some(u64::from(RETENTION_INCREMENTAL_VACUUM_PAGES))
    );
    println!("[m10-11] status.capacity = {capacity:?}");
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

#[test]
fn archive_failure_skips_batch_and_keeps_rows() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
    let shard_path = layout
        .shard_path(shard_for(SESSION_SHARD0))
        .expect("shard")
        .to_path_buf();
    insert_events(&shard_path, SESSION_SHARD0, 1, 12, "log", OLD_TS);
    insert_audit(layout.main_db());

    let (storage, runtime) = runtime_for(layout.clone());
    enable_retention(&storage, &runtime);
    let task = retention_task(
        &storage,
        dir.path(),
        Arc::new(MaintenanceCell::new()),
        Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    );
    // 用普通文件占位 `archives/`，使 `create_dir_all` 失败（归档失败 → 跳过该批）。
    std::fs::write(dir.path().join("archives"), b"blocked").expect("block archives dir");
    let failed = runtime.block_on(task.run_once());
    assert!(matches!(failed.outcome, RetentionOutcome::Failed { .. }));
    assert_eq!(
        shard_count(
            storage.reads(),
            &runtime,
            shard_for(SESSION_SHARD0),
            "SELECT COUNT(*) FROM events"
        ),
        12,
        "归档失败必须保留该批（不得先删）"
    );
    assert_eq!(
        count(
            storage.reads(),
            &runtime,
            "SELECT COUNT(*) FROM audit_log WHERE detail = 'must-survive-retention'"
        ),
        1
    );

    std::fs::remove_file(dir.path().join("archives")).expect("unblock");
    let success = runtime.block_on(task.run_once());
    match &success.outcome {
        RetentionOutcome::Cleaned { rows, .. } => assert_eq!(*rows, 12),
        other => panic!("期望 Cleaned，实际 {other:?}"),
    }
    assert_eq!(
        shard_count(
            storage.reads(),
            &runtime,
            shard_for(SESSION_SHARD0),
            "SELECT COUNT(*) FROM events"
        ),
        0
    );
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

#[test]
fn cleanup_defers_while_maintenance_state_is_active() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
    let shard_path = layout
        .shard_path(shard_for(SESSION_SHARD0))
        .expect("shard")
        .to_path_buf();
    insert_events(&shard_path, SESSION_SHARD0, 1, 3, "log", OLD_TS);
    let (storage, runtime) = runtime_for(layout.clone());
    enable_retention(&storage, &runtime);
    let maintenance = Arc::new(MaintenanceCell::new());
    maintenance.set(Some(aether_app::ipc::MaintenanceState {
        kind: "backup".to_owned(),
        reason: "测试".to_owned(),
        started_at: NOW_MS,
        progress: None,
    }));
    let task = retention_task(
        &storage,
        dir.path(),
        Arc::clone(&maintenance),
        Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    );
    let report = runtime.block_on(task.run_once());
    assert!(matches!(report.outcome, RetentionOutcome::Deferred { .. }));
    assert_eq!(
        shard_count(
            storage.reads(),
            &runtime,
            shard_for(SESSION_SHARD0),
            "SELECT COUNT(*) FROM events"
        ),
        3
    );
    maintenance.set(None);
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

#[test]
fn maintenance_vacuum_precheck_space_and_warning_are_enforced() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
    let shard_id = shard_for(SESSION_SHARD0);
    let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
    // 填充并删除 50k 行，制造 freelist（VACUUM 才能观察到物理收缩）。
    insert_events(&shard_path, SESSION_SHARD0, 1, 50_000, "log", OLD_TS);

    let (storage, runtime) = runtime_for(layout.clone());
    runtime.block_on(async {
        storage
            .queue()
            .execute(StoreCommand::DeleteEventsInShard {
                shard_id,
                rowids: (1..=50_000).collect(),
            })
            .await
            .expect("delete seeded rows");
        storage
            .queue()
            .checkpoint_truncate(aether_store::CheckpointConfig::default())
            .await
            .expect("checkpoint");
    });
    let physical_before = generation_live_bytes(&layout);

    let maintenance = Arc::new(MaintenanceCell::new());
    let rejecting = retention_task(
        &storage,
        dir.path(),
        Arc::clone(&maintenance),
        Arc::new(FixedSpaceProbe { available: 0 }),
    );
    let error = runtime
        .block_on(rejecting.run_maintenance_vacuum(Some(1)))
        .expect_err("空间不足必须拒绝");
    assert!(
        error.message.contains("空间预检失败"),
        "错误必须标明空间预检：{error}"
    );
    assert_eq!(
        generation_live_bytes(&layout),
        physical_before,
        "拒绝后当前库不变"
    );

    let task = retention_task(
        &storage,
        dir.path(),
        Arc::clone(&maintenance),
        Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    );
    let report = runtime
        .block_on(task.run_maintenance_vacuum(Some(1)))
        .expect("维护态 VACUUM");
    assert!(report.required && report.performed);
    assert!(
        report.physical_after < report.physical_before,
        "维护态 VACUUM 必须收缩物理文件：{physical_before} → {}",
        report.physical_after
    );
    assert!(report.stop_write_ms <= report.took_ms + 1_000);
    assert!(
        report.warning.is_some(),
        "VACUUM 后仍高于注入阈值 1 字节 → 必须告警"
    );
    assert!(
        maintenance.get().is_none(),
        "维护态必须在 VACUUM 完成后清除"
    );
    let status = runtime.block_on(task.status()).expect("status");
    assert!(status["vacuum"]["last_maintenance"].is_object());
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

fn external_generation() -> (tempfile::TempDir, StorageLayout) {
    let source = std::env::var_os("AETHER_M10_11_GENERATION")
        .expect("需要 AETHER_M10_11_GENERATION 指向 5.22GiB generation 的副本");
    if std::env::var("AETHER_M10_11_GENERATION_IS_COPY").as_deref() == Ok("1") {
        let holder = tempfile::tempdir().expect("holder");
        let layout = aether_store::read_layout(Path::new(&source)).expect("generation layout");
        return (holder, layout);
    }
    let root = std::env::var_os("AETHER_M10_11_BENCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).expect("bench root");
    let dir = tempfile::Builder::new()
        .prefix("m10-11-generation-copy-")
        .tempdir_in(&root)
        .expect("temp dir");
    copy_directory(Path::new(&source), dir.path());
    let layout = aether_store::read_layout(dir.path()).expect("generation layout");
    (dir, layout)
}

fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("target dir");
    for entry in std::fs::read_dir(source).expect("read source") {
        let entry = entry.expect("entry");
        let target_path = target.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target_path);
        } else {
            std::fs::copy(entry.path(), &target_path).expect("copy file");
        }
    }
}

fn write_evidence(name: &str, value: &serde_json::Value) {
    println!("[m10-11] {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M10_11_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(dir.join(format!("{name}.json")), text);
    }
}

#[test]
#[ignore = "需要 5.22GiB generation 副本（AETHER_M10_11_GENERATION）与真实磁盘"]
fn full_history_cleanup_on_standard_generation() {
    let (_dir, layout) = external_generation();
    let (storage, runtime) = runtime_for(layout.clone());
    runtime.block_on(async {
        for (key, value) in [
            ("events.retention.enabled", "true"),
            ("events.retention.days", "30"),
        ] {
            storage
                .queue()
                .execute(StoreCommand::UpsertSetting {
                    key: key.to_owned(),
                    value: value.to_owned(),
                    updated_at: 2_000_000_000_000,
                })
                .await
                .expect("retention setting");
        }
    });
    let clock: SharedClock = Arc::new(ManualClock::new(2_000_000_000_000));
    let task = RetentionTask::new(RetentionDeps {
        data_dir: layout.data_dir().to_path_buf(),
        reads: storage.reads().clone(),
        write: storage.queue().clone(),
        clock,
        thresholds: RetentionThresholds::default(),
        maintenance: Some(Arc::new(MaintenanceCell::new())),
        space: Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    });
    let started = std::time::Instant::now();
    let report = runtime.block_on(task.run_once());
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let status = runtime.block_on(task.status()).expect("status");
    let result = serde_json::json!({
        "task": "M10-11",
        "scenario": "full_history_cleanup",
        "outcome": format!("{:?}", report.outcome),
        "elapsed_ms": elapsed_ms,
        "hard_budget_ms": 4 * 60 * 60 * 1000u64,
        "within_budget": elapsed_ms <= 4 * 60 * 60 * 1000,
        "status": status,
    });
    write_evidence("full_history_cleanup", &result);
    match &report.outcome {
        RetentionOutcome::Cleaned { rows, .. } => {
            assert!(*rows > 0, "标准数据集必须清理出白名单行")
        }
        other => panic!("期望 Cleaned，实际 {other:?}"),
    }
    assert!(
        elapsed_ms <= 4 * 60 * 60 * 1000,
        "全量历史清理必须 ≤4h：{elapsed_ms}ms"
    );
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

#[test]
#[ignore = "1M 行合成数据（日常增量 5min 预算）；约数分钟"]
fn daily_incremental_cleanup_within_five_minutes() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", OLD_TS).expect("init");
    let shard_path = layout
        .shard_path(shard_for(SESSION_SHARD0))
        .expect("shard")
        .to_path_buf();
    insert_events(&shard_path, SESSION_SHARD0, 1, 1_000_000, "log", OLD_TS);
    let (storage, runtime) = runtime_for(layout.clone());
    enable_retention(&storage, &runtime);
    let task = retention_task(
        &storage,
        dir.path(),
        Arc::new(MaintenanceCell::new()),
        Arc::new(FixedSpaceProbe {
            available: u64::MAX / 2,
        }),
    );
    let started = std::time::Instant::now();
    let report = runtime.block_on(task.run_once());
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let result = serde_json::json!({
        "task": "M10-11",
        "scenario": "daily_incremental_cleanup",
        "rows": 1_000_000,
        "outcome": format!("{:?}", report.outcome),
        "elapsed_ms": elapsed_ms,
        "hard_budget_ms": 5 * 60 * 1000u64,
        "within_budget": elapsed_ms <= 5 * 60 * 1000,
    });
    write_evidence("daily_incremental_cleanup", &result);
    match &report.outcome {
        RetentionOutcome::Cleaned { rows, .. } => assert_eq!(*rows, 1_000_000),
        other => panic!("期望 Cleaned，实际 {other:?}"),
    }
    assert!(
        elapsed_ms <= 5 * 60 * 1000,
        "日常增量清理必须 ≤5min：{elapsed_ms}ms"
    );
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}

#[test]
#[ignore = "需要 5.22GiB generation 副本与真实磁盘（维护态 VACUUM 约数分钟）"]
fn maintenance_vacuum_on_standard_generation() {
    let (_dir, layout) = external_generation();
    let (storage, runtime) = runtime_for(layout.clone());
    let task = RetentionTask::new(RetentionDeps {
        data_dir: layout.data_dir().to_path_buf(),
        reads: storage.reads().clone(),
        write: storage.queue().clone(),
        clock: Arc::new(ManualClock::new(NOW_MS)),
        thresholds: RetentionThresholds::default(),
        maintenance: Some(Arc::new(MaintenanceCell::new())),
        space: Arc::new(aether_app::backup_control::NativeSpaceProbe),
    });
    let physical_before = generation_live_bytes(&layout);
    let status_before = runtime.block_on(task.status()).expect("status before");
    assert!(
        status_before["capacity"]["physical_warning"].is_string(),
        "物理 >5GiB 必须告警：{status_before}"
    );
    let report = runtime
        .block_on(task.run_maintenance_vacuum(None))
        .expect("维护态 VACUUM");
    let status_after = runtime.block_on(task.status()).expect("status after");
    let result = serde_json::json!({
        "task": "M10-11",
        "scenario": "maintenance_vacuum_standard_generation",
        "physical_before": physical_before,
        "status_before": status_before,
        "status_after": status_after,
        "report": report.to_json(),
    });
    write_evidence("maintenance_vacuum_standard", &result);
    assert!(
        physical_before > 5 * 1024 * 1024 * 1024,
        "标准数据集物理应 >5GiB：{physical_before}"
    );
    assert!(
        report.required && report.performed,
        "超过 5GiB 必须执行 VACUUM"
    );
    assert!(
        report.required_space_bytes >= physical_before.saturating_mul(2),
        "空间预检必须 ≥ db+wal ×2"
    );
    runtime.block_on(async { storage.shutdown().await.expect("shutdown") });
}
