//! M10-11（ADR-039 v1.0 D-039-4）保留清理与文件收缩——store 层验收。
//!
//! 覆盖：
//! - `(type, ts)` 选批执行计划无 `TEMP B-TREE`（`idx_events_type_ts`）；
//! - 单批成员按 `(type, ts)` 排序、最终清理集合不变、`audit_log` 不清理；
//! - 新装主库/分片 `auto_vacuum=INCREMENTAL`；有界 `incremental_vacuum` 回收物理空间。
//!
//! 删除事务 500/1000/2000/5000 档位矩阵与 P95 由 `m10_11_bench`（release）测量。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_store::archive::{
    archive_retention_batch, explain_retention_batch, select_retention_batch_generation,
    verify_archive_file, RETENTION_SELECT_SQL,
};
use aether_store::layout::{
    initialize as initialize_layout, shard_for, StorageLayout, SHARD_COUNT,
};
use aether_store::{
    generation_capacity, generation_live_bytes, StoreCommand, StoreRuntime, WriteQueueConfig,
};
use rusqlite::{params, Connection};
use serde_json::Value;

const SESSION_SHARD0: &str = "01J8ZQ00000000000000000007";

fn identity_sanitize(value: &Value) -> Result<String, aether_store::StoreError> {
    serde_json::to_string(value).map_err(|error| aether_store::StoreError::Internal {
        reason: error.to_string(),
    })
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
        _ => format!(r#"{{"index":{index}}}"#),
    }
}

fn insert_events(
    path: &Path,
    session_id: &str,
    start_seq: u64,
    rows: usize,
    event_type: &str,
    ts: i64,
) {
    let mut connection = Connection::open(path).expect("open fixture db");
    let transaction = connection.transaction().expect("fixture txn");
    for index in 0..rows {
        let seq = start_seq.saturating_add(u64::try_from(index).unwrap_or(0));
        let id = format!("01J8ZQ{seq:020}");
        transaction
            .execute(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES (?1, ?2, NULL, 'mock', ?3, ?4, ?5, ?6, 1)",
                params![
                    id,
                    session_id,
                    i64::try_from(seq).unwrap_or(i64::MAX),
                    event_type,
                    payload_for(event_type, index),
                    ts
                ],
            )
            .expect("insert fixture event");
    }
    transaction.commit().expect("fixture commit");
}

fn insert_audit(path: &Path) {
    let connection = Connection::open(path).expect("open main db");
    connection
        .execute(
            "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, \
             result, ts) VALUES ('01JAUDIT000000000000000001', NULL, NULL, 'system', \
             'retention.test', NULL, 'must-survive-retention', 'success', 1)",
            [],
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

fn count_rows(path: &Path, sql: &str) -> i64 {
    let connection = Connection::open(path).expect("open count db");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count")
}

#[test]
fn explain_retention_batch_uses_type_ts_index_without_temp_btree() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1_700_000_000_000).expect("init");
    let shard = layout.shard_path(0).expect("shard 0");
    let connection = Connection::open_with_flags(shard, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open shard read-only");
    let plan = explain_retention_batch(&connection, 1_750_000_000_000, 5_000).expect("explain");
    let joined = plan.join(" | ");
    assert!(
        joined.contains("idx_events_type_ts"),
        "选批必须命中 idx_events_type_ts：{joined}"
    );
    assert!(
        !joined.to_ascii_uppercase().contains("TEMP B-TREE"),
        "选批不得使用 TEMP B-TREE：{joined}"
    );
    assert!(
        !RETENTION_SELECT_SQL.contains("ORDER BY rowid"),
        "产品选批不得再使用 ORDER BY rowid：{RETENTION_SELECT_SQL}"
    );
    assert!(
        RETENTION_SELECT_SQL.contains("ORDER BY type ASC, ts ASC"),
        "产品选批必须按 (type, ts) 排序：{RETENTION_SELECT_SQL}"
    );
    println!("[m10-11] explain_retention_batch = {joined}");
}

#[test]
fn retention_batch_order_and_final_cleanup_set_are_stable() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1_700_000_000_000).expect("init");
    let shard_id = shard_for(SESSION_SHARD0);
    assert_eq!(shard_id, 0, "固定测试向量：SESSION_SHARD0 → 分片 0");
    let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
    // 30 条超窗白名单（三类型交错 ts）+ 5 条控制事件 + 5 条窗口内白名单。
    insert_events(&shard_path, SESSION_SHARD0, 1, 30, "log", 1_700_000_000_000);
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        31,
        30,
        "message.delta",
        1_700_000_000_001,
    );
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        61,
        30,
        "usage",
        1_700_000_000_002,
    );
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        91,
        5,
        "run.started",
        1_700_000_000_003,
    );
    insert_events(
        &shard_path,
        SESSION_SHARD0,
        96,
        5,
        "message.delta",
        1_800_000_000_000,
    );
    insert_audit(layout.main_db());

    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    let archive_dir = dir.path().join("archives");
    std::fs::create_dir_all(&archive_dir).expect("archives dir");
    let cutoff = 1_750_000_000_000;
    let mut batches_seen = 0usize;
    let mut first_types: Vec<String> = Vec::new();
    let mut deleted = 0usize;
    runtime.block_on(async {
        loop {
            let batches =
                select_retention_batch_generation(&layout, cutoff, 7).expect("select batch");
            if batches.is_empty() {
                break;
            }
            for (shard_id, batch) in batches {
                if batches_seen == 0 {
                    first_types = batch.iter().map(|row| row.event_type.clone()).collect();
                    let mut sorted = batch.clone();
                    sorted.sort_by(|left, right| {
                        left.event_type
                            .cmp(&right.event_type)
                            .then(left.ts.cmp(&right.ts))
                    });
                    assert_eq!(
                        batch.iter().map(|row| row.rowid).collect::<Vec<_>>(),
                        sorted.iter().map(|row| row.rowid).collect::<Vec<_>>(),
                        "单批成员必须按 (type, ts) 排序"
                    );
                }
                batches_seen += 1;
                let report = archive_retention_batch(
                    &archive_dir,
                    &batch,
                    1_800_000_000_000,
                    &identity_sanitize,
                )
                .expect("archive batch");
                let verification = verify_archive_file(&report.path).expect("verify archive");
                assert!(verification.matches, "归档 manifest/payload 必须一致");
                let rowids: Vec<i64> = batch.iter().map(|row| row.rowid).collect();
                let row_count = rowids.len();
                queue
                    .execute(StoreCommand::DeleteEventsInShard { shard_id, rowids })
                    .await
                    .expect("delete batch");
                deleted += row_count;
                tokio::task::yield_now().await;
            }
        }
        storage.shutdown().await.expect("shutdown");
    });

    assert_eq!(deleted, 90, "最终清理集合必须等于全部超窗白名单行");
    assert!(
        batches_seen >= 13,
        "批上限 7 时应分成 ≥13 批：{batches_seen}"
    );
    assert!(
        first_types.iter().all(|value| value == "log"),
        "首批应连续取 type 最小的 log：{first_types:?}"
    );
    let remaining: i64 = count_rows(&shard_path, "SELECT COUNT(*) FROM events");
    assert_eq!(remaining, 10, "控制事件 5 + 窗口内白名单 5 必须保留");
    assert_eq!(
        count_rows(
            &shard_path,
            "SELECT COUNT(*) FROM events WHERE type IN ('message.delta','usage','log') \
             AND ts < 1750000000000"
        ),
        0,
        "超窗白名单行必须全部清理"
    );
    assert_eq!(
        count_rows(layout.main_db(), "SELECT COUNT(*) FROM audit_log"),
        1,
        "audit_log 不清理"
    );
    let audit_detail: String = Connection::open(layout.main_db())
        .expect("main")
        .query_row("SELECT detail FROM audit_log LIMIT 1", [], |row| row.get(0))
        .expect("audit detail");
    assert_eq!(audit_detail, "must-survive-retention");
}

#[test]
fn new_generation_uses_incremental_auto_vacuum_and_bounded_reclaim() {
    let dir = tempfile::tempdir().expect("temp");
    let layout = initialize_layout(dir.path(), "0.1.0", 1_700_000_000_000).expect("init");
    assert_eq!(layout.shard_paths().len(), SHARD_COUNT);
    for path in layout.all_db_paths() {
        let connection = Connection::open(path).expect("open db");
        let auto_vacuum: i64 = connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .expect("auto_vacuum");
        assert_eq!(auto_vacuum, 2, "新装主库/分片 auto_vacuum=INCREMENTAL");
    }

    let shard_id = shard_for(SESSION_SHARD0);
    let shard_path = layout.shard_path(shard_id).expect("shard").to_path_buf();
    // 填充约 20MB（每行 payload ~400B，50k 行）后删除，制造 freelist。
    let mut connection = Connection::open(&shard_path).expect("shard");
    let transaction = connection.transaction().expect("seed txn");
    for index in 0..50_000u64 {
        let seq = index + 1;
        let payload = format!(r#"{{"level":"info","message":"{}"}}"#, "x".repeat(400));
        transaction
            .execute(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES (?1, ?2, NULL, 'mock', ?3, 'log', ?4, 1, 1)",
                params![format!("01J8ZQ{seq:020}"), SESSION_SHARD0, i64::try_from(seq).unwrap_or(0), payload],
            )
            .expect("seed row");
    }
    transaction.commit().expect("seed commit");
    drop(connection);

    let (storage, runtime) = runtime_for(layout.clone());
    let queue = storage.queue().clone();
    let physical_before = generation_live_bytes(&layout);
    let capacity_before = generation_capacity(&layout).expect("capacity before");
    assert!(capacity_before.live_bytes > 0);
    runtime.block_on(async {
        queue
            .execute(StoreCommand::DeleteEventsInShard {
                shard_id,
                rowids: (1..=50_000).collect(),
            })
            .await
            .expect("delete seeded rows");
        queue
            .checkpoint_truncate(aether_store::CheckpointConfig::default())
            .await
            .expect("checkpoint");
        queue
            .execute(StoreCommand::IncrementalVacuum { pages: 4_096 })
            .await
            .expect("incremental vacuum");
        let report = queue
            .checkpoint_truncate(aether_store::CheckpointConfig::default())
            .await
            .expect("checkpoint after vacuum");
        assert!(
            report.succeeded,
            "incremental_vacuum 后 checkpoint 必须成功"
        );
        storage.shutdown().await.expect("shutdown");
    });
    let physical_after = generation_live_bytes(&layout);
    let capacity_after = generation_capacity(&layout).expect("capacity after");
    println!(
        "[m10-11] incremental_vacuum physical_before={physical_before} physical_after={physical_after} \
         free_before={} free_after={}",
        capacity_before.free_bytes, capacity_after.free_bytes
    );
    assert!(
        physical_after < physical_before,
        "有界 incremental_vacuum 必须回收物理空间：{physical_before} → {physical_after}"
    );
    assert!(
        count_rows(&shard_path, "SELECT COUNT(*) FROM events") == 0,
        "删除后无残留事件"
    );
}
