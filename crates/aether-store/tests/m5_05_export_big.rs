//! M5-05 DoD2（大库抽样，≥1GB）：全库归档在 ≥1GB 库上的完整性验证。
//!
//! 机制：构造 ≥1GB 的 `events` 表（100 万行 × ~1KB payload；分批提交 +
//! `wal_checkpoint(TRUNCATE)` 控制 WAL 峰值），执行全库归档 →
//! [`verify_archive_file`] 流式校验（manifest 行数/sha256）→ 首/末事件行与库内
//! 逐字段抽样比对；记录耗时/吞吐/库体积。
//!
//! 由 `scripts/test/m5-05/verify-m5-05.mjs` 以
//! `AETHER_M5_05_BIG=1 cargo test --release -p aether-store --test m5_05_export_big -- --ignored`
//! 启用（重 IO/耗时，不进入默认回归）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

use aether_store::archive::{export_full_archive, verify_archive_file};
use aether_store::{Store, StoreError};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J0000000000000000000000A";
const TOTAL_EVENTS: i64 = 1_000_000;
const BATCH_ROWS: i64 = 100_000;

fn identity_sanitize(value: &Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::Internal {
        reason: error.to_string(),
    })
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m5-05-big] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_05_EVIDENCE_DIR") else {
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

/// 构造 ≥1GB 库（分批提交 + checkpoint 控制 WAL 峰值）。
fn seed_big_database(store: &Store) {
    let connection = store.connection();
    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES (?1, 'mock', '大库抽样', 'idle', 10, 10)",
            [SESSION_ID],
        )
        .unwrap();
    let mut start = 1i64;
    while start <= TOTAL_EVENTS {
        let end = (start + BATCH_ROWS - 1).min(TOTAL_EVENTS);
        // payload ≈ 1KB/行（至 ~1.1KB）；id/seq 连续；ts 固定。
        let sql = format!(
            "WITH RECURSIVE seqs(i) AS (SELECT {start} UNION ALL SELECT i + 1 FROM seqs WHERE i < {end}) \
             INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             SELECT printf('01J%023d', i), '{SESSION_ID}', NULL, 'mock', i, 'message.delta', \
                    '{{\"message_id\":\"01J000000000000000000000M1\",\"text\":\"' || substr(hex(zeroblob(500)), 1, 1000) || '\"}}', \
                    1700000000000, 1 \
             FROM seqs;"
        );
        connection
            .execute_batch(&format!("BEGIN; {sql} COMMIT;"))
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        start = end + 1;
    }
}

#[test]
#[ignore = "≥1GB 大库抽样：由 verify-m5-05 以 AETHER_M5_05_BIG=1 启用（release）"]
fn big_database_export_is_complete_and_verified() {
    let dir = common::temp_dir("m5-05-big");
    let db_path = common::db_path(&dir);
    let store = Store::open(&db_path).unwrap();
    let seed_started = Instant::now();
    seed_big_database(&store);
    let seed_elapsed = seed_started.elapsed();
    store
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let db_bytes = std::fs::metadata(&db_path).unwrap().len();
    assert!(
        db_bytes >= 1024 * 1024 * 1024,
        "大库样本必须 ≥1GB：实际 {db_bytes} 字节"
    );

    let archive_dir = dir.path().join("archives");
    std::fs::create_dir_all(&archive_dir).unwrap();
    let export_started = Instant::now();
    let report = export_full_archive(
        store.connection(),
        &archive_dir,
        1_800_000_000_000,
        &identity_sanitize,
    )
    .unwrap();
    let export_elapsed = export_started.elapsed();
    assert_eq!(
        report.rows,
        u64::try_from(TOTAL_EVENTS).unwrap() + 1,
        "payload 行数 = 事件 100 万 + 会话 1"
    );
    assert_eq!(report.counts.events, u64::try_from(TOTAL_EVENTS).unwrap());

    // 流式校验 + 首/末事件行抽样（单次扫描，不整库入内存）。
    let verify_started = Instant::now();
    let verification = verify_archive_file(&report.path).unwrap();
    assert!(
        verification.matches,
        "manifest 行数/sha256 必须与 payload 一致"
    );
    let verify_elapsed = verify_started.elapsed();

    let file = std::fs::File::open(&report.path).unwrap();
    let reader = BufReader::with_capacity(1024 * 1024, file);
    let mut first_event_line: Option<String> = None;
    let mut last_line: Option<String> = None;
    for line in reader.lines() {
        let line = line.unwrap();
        if first_event_line.is_none() {
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["kind"] == "event" {
                first_event_line = Some(line.clone());
            }
        }
        last_line = Some(line);
    }
    let first: Value = serde_json::from_str(first_event_line.as_deref().unwrap()).unwrap();
    let last: Value = serde_json::from_str(last_line.as_deref().unwrap()).unwrap();
    assert_eq!(first["seq"], 1);
    assert_eq!(first["id"], format!("01J{:023}", 1));
    assert_eq!(last["seq"], TOTAL_EVENTS);
    assert_eq!(last["id"], format!("01J{:023}", TOTAL_EVENTS));
    // 与库内逐字段抽样比对（首/末行 payload 与库内一致）。
    for seq in [1i64, TOTAL_EVENTS] {
        let expected_id = format!("01J{seq:023}");
        let (db_id, db_payload): (String, String) = store
            .connection()
            .query_row(
                "SELECT id, payload FROM events WHERE seq = ?1",
                [seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(db_id, expected_id);
        let archive_payload = if seq == 1 {
            &first["payload"]
        } else {
            &last["payload"]
        };
        assert_eq!(
            serde_json::from_str::<Value>(&db_payload).unwrap(),
            *archive_payload,
            "seq={seq} payload 与库内一致"
        );
    }

    write_evidence(
        "dod2_big_database",
        &json!({
            "db_bytes": db_bytes,
            "db_gib": (db_bytes as f64) / (1024.0 * 1024.0 * 1024.0),
            "events": TOTAL_EVENTS,
            "archive_bytes": report.bytes,
            "archive_rows": report.rows,
            "archive_matches": verification.matches,
            "seed_elapsed_ms": seed_elapsed.as_millis(),
            "export_elapsed_ms": export_elapsed.as_millis(),
            "verify_elapsed_ms": verify_elapsed.as_millis(),
        }),
    );
}
