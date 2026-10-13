//! M5-04 DoD5（store 级）：保留清理删除半步的精确性与长事务口径。
//!
//! 覆盖：
//! - 批选择（白名单 + 窗口）→ 先归档（写入 + 校验）→ 按 `rowid` 精确删除；
//! - 删除仅影响归档过的行（控制事件/窗口内事件逐行保留）；
//! - 单事务耗时记录（批 ≤5000 行；release 基准断言 <500ms，debug 构建仅记录——
//!   同 M1-04/M5-02 口径，bundled SQLite 在 debug 为 -O0）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aether_core::EventEnvelope;
use aether_store::archive::{archive_retention_batch, select_retention_batch, verify_archive_file};
use aether_store::{StoreCommand, StoreError, StoreRuntime, WriteQueue, WriteQueueConfig};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J0000000000000000000000A";
const OLD_TS: i64 = 1_700_000_000_000;

fn event_json(id: &str, seq: u64, event_type: &str, payload: &str, ts: i64) -> String {
    format!(
        r#"{{"v":1,"id":"{id}","session_id":"{SESSION_ID}","run_id":null,"runtime_id":"mock","seq":{seq},"ts":{ts},"type":"{event_type}","payload":{payload}}}"#
    )
}

fn identity_sanitize(value: &Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::Internal {
        reason: error.to_string(),
    })
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m5-04-ops] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_04_EVIDENCE_DIR") else {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_batch_delete_is_exact_and_bounded() {
    let dir = common::temp_dir("m5-04-ops");
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let reads = runtime.reads().clone();
    let queue: WriteQueue = runtime.queue().clone();

    // 种子：5000 超窗白名单 delta + 500 超窗控制事件（run.started）+ 500 窗口内 delta。
    let mut envelopes: Vec<EventEnvelope> = Vec::with_capacity(6_000);
    for index in 0..5_000u64 {
        envelopes.push(
            EventEnvelope::from_json_str(&event_json(
                &format!("01JW{index:023}"),
                index + 1,
                "message.delta",
                "{\"message_id\":\"01J000000000000000000000M1\",\"text\":\"chunk\"}",
                OLD_TS,
            ))
            .unwrap(),
        );
    }
    for index in 0..500u64 {
        envelopes.push(
            EventEnvelope::from_json_str(&event_json(
                &format!("01JC{index:023}"),
                5_001 + index,
                "run.started",
                "{\"run_id\":\"01J000000000000000000000R1\"}",
                OLD_TS,
            ))
            .unwrap(),
        );
    }
    for index in 0..500u64 {
        envelopes.push(
            EventEnvelope::from_json_str(&event_json(
                &format!("01JN{index:023}"),
                5_501 + index,
                "message.delta",
                "{\"message_id\":\"01J000000000000000000000M1\",\"text\":\"chunk\"}",
                1_800_000_000_000,
            ))
            .unwrap(),
        );
    }
    let receipt = queue.append_events(envelopes).await.unwrap();
    assert_eq!(receipt.entries, 6_000);

    // 批选择：仅 5000 条超窗白名单事件。
    let cutoff = 1_750_000_000_000;
    let batch = reads
        .with_connection(move |connection| select_retention_batch(connection, cutoff, 5_000))
        .await
        .unwrap();
    assert_eq!(batch.len(), 5_000);
    assert!(batch
        .iter()
        .all(|row| row.event_type == "message.delta" && row.ts < cutoff));

    // 先归档并校验（删除前置条件）。
    let archive =
        archive_retention_batch(dir.path(), &batch, 1_800_000_000_000, &identity_sanitize).unwrap();
    let verification = verify_archive_file(&archive.path).unwrap();
    assert!(verification.matches);
    assert_eq!(verification.rows, 5_000);

    // 删除（单命令单事务）：计时记录 + 精确影响行数。
    let rowids: Vec<i64> = batch.iter().map(|row| row.rowid).collect();
    let started = Instant::now();
    let outcome = queue
        .execute(StoreCommand::DeleteEventsByIds { rowids })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        outcome,
        aether_store::StoreOutcome::Applied { affected: 5_000 }
    );

    // 剩余 = 500 控制事件 + 500 窗口内 delta；归档内容 == 被删行（抽样断言）。
    let remaining = reads
        .with_connection(move |connection| {
            let count: i64 =
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            let control: i64 = connection.query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'run.started'",
                [],
                |row| row.get(0),
            )?;
            let in_window: i64 = connection.query_row(
                "SELECT COUNT(*) FROM events WHERE ts >= ?1",
                [cutoff],
                |row| row.get(0),
            )?;
            Ok::<_, StoreError>((count, control, in_window))
        })
        .await
        .unwrap();
    assert_eq!(remaining, (1_000, 500, 500), "仅归档过的白名单超窗行被删除");
    let archived_ids: Vec<String> = std::fs::read_to_string(&archive.path)
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            value["id"].as_str().map(str::to_owned)
        })
        .collect();
    assert_eq!(archived_ids.len(), 5_000);
    assert!(
        archived_ids.iter().all(|id| id.starts_with("01JW")),
        "归档必须是删除精确行集"
    );

    write_evidence(
        "dod5_delete_transaction",
        &json!({
            "deleted_rows": 5_000,
            "delete_elapsed_ms": elapsed.as_millis(),
            "archive_rows": verification.rows,
            "archive_matches": verification.matches,
            "remaining_total": remaining.0,
            "remaining_control": remaining.1,
            "remaining_in_window": remaining.2,
            "debug_build": cfg!(debug_assertions),
            "hard_threshold_ms": 500,
            "hard_threshold_met": elapsed < Duration::from_millis(500),
        }),
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "单事务必须 <500ms（D3/ADR-021 §2.1-5）：{elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
}
