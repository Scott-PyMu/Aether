//! M10-04 导入（ADR-034）—— store 层验收。
//!
//! 覆盖：
//! - JSONL/manifest 校验、哈希/行数不一致、超限行、未知 kind/event、密钥字段；
//! - staging 快照 + remap/preserve + seq 重排 + client_msg_id 去重；
//! - 启动原子切换、幂等标记、`.pre-import-*` 现场保护；
//! - 失败/取消/中断不改变既有数据；staging 清理；
//! - 往返（导出 → 导入 → 再导出）行数/关键字段一致。
//!
//! 大文件样本（≥1GB）为 `#[ignore]`，由 `AETHER_M10_04_BIG_MIB` 驱动：
//! `AETHER_M10_04_BIG_MIB=1024 cargo test -p aether-store --test m10_04_import -- --ignored big_import_sample_is_streamed`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use aether_store::archive::{
    export_full_archive, export_session_archive, verify_archive_file, ArchiveReport,
};
use aether_store::import::{
    apply_source_to_staging, boot_apply_pending_import, cleanup_imports, create_staging_snapshot,
    has_marker, job_path, mark_staging, pending_exists, precheck_source,
    reconcile_interrupted_imports, remove_pending, validate_source, verify_staging, work_db_path,
    write_job, write_pending, ImportFailureKind, ImportJob, ImportJobStatus, ImportManifest,
    ImportMode, ImportSummary, ARCHIVE_IMPORT_SOURCES, SESSION_IMPORT_SOURCES,
};
use aether_store::{Store, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SESSION_ID: &str = "01J0000000000000000000000A";
const MESSAGE_USER: &str = "01J000000000000000000000M1";
const MESSAGE_ASSISTANT: &str = "01J000000000000000000000M2";
const EVENT_1: &str = "01J000000000000000000000E1";
const EVENT_2: &str = "01J000000000000000000000E2";
const RUN_ID: &str = "01J000000000000000000000R1";
const AUDIT_ID: &str = "01J000000000000000000000U1";

fn temp_dir(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("aether-m10-04-{label}-"))
        .tempdir()
        .expect("临时目录")
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
        )
        .expect("runtime");
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES (?1, 'mock', '源会话', 'idle', 10, 10)",
            [SESSION_ID],
        )
        .expect("session");
    connection
        .execute(
            "INSERT INTO messages (id, session_id, role, content, seq, created_at, client_msg_id) \
             VALUES (?1, ?2, 'user', '你好', 10, 11, 'client-1'), \
                    (?3, ?2, 'assistant', '回复', 20, 12, NULL)",
            params![MESSAGE_USER, SESSION_ID, MESSAGE_ASSISTANT],
        )
        .expect("messages");
    connection
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES (?1, ?2, NULL, 'mock', 10, 'session.created', '{\"title\":\"源会话\"}', 10, 1), \
                    (?3, ?2, NULL, 'mock', 20, 'message.completed', '{\"message_id\":\"m1\"}', 12, 1)",
            params![EVENT_1, SESSION_ID, EVENT_2],
        )
        .expect("events");
    connection
        .execute(
            "INSERT INTO runs (id, session_id, status, input_message_id, started_at, finished_at) \
             VALUES (?1, ?2, 'succeeded', ?3, 11, 12)",
            params![RUN_ID, SESSION_ID, MESSAGE_USER],
        )
        .expect("run");
    connection
        .execute(
            "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
             VALUES (?1, ?2, 'mock', 'user', 'session.exported', NULL, '{\"rows\":3}', 'success', 13)",
            params![AUDIT_ID, SESSION_ID],
        )
        .expect("audit");
}

fn seed_database(path: &Path) {
    {
        let store = Store::open(path).expect("建库");
        drop(store);
    }
    let connection = Connection::open(path).expect("连接");
    seed(&connection);
}

fn identity(value: &Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::Internal {
        reason: error.to_string(),
    })
}

fn export_session(db: &Path, dir: &Path) -> ArchiveReport {
    let connection = Connection::open(db).expect("只读连接");
    export_session_archive(&connection, dir, SESSION_ID, 1_700_000_000_000, &identity)
        .expect("导出")
        .expect("会话存在")
}

fn manifest_line(source: &str, session_id: Option<&str>, rows: &[String]) -> Value {
    let mut hasher = Sha256::new();
    for row in rows {
        hasher.update(row.as_bytes());
        hasher.update(b"\n");
    }
    json!({
        "v": 1,
        "kind": "manifest",
        "created_at": 1_700_000_000_000i64,
        "source": source,
        "session_id": session_id,
        "rows": rows.len(),
        "sha256": hex::encode(hasher.finalize()),
    })
}

fn write_jsonl(path: &Path, source: &str, session_id: Option<&str>, rows: Vec<Value>) -> Vec<u8> {
    let mut lines = Vec::new();
    for row in &rows {
        lines.push(serde_json::to_string(row).expect("行序列化"));
    }
    let manifest = manifest_line(source, session_id, &lines);
    let mut text = serde_json::to_string(&manifest).expect("manifest") + "\n";
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(path, &text).expect("写 JSONL");
    text.into_bytes()
}

fn session_row(id: &str) -> Value {
    json!({
        "v": 1,
        "kind": "session",
        "id": id,
        "runtime_id": "mock",
        "workspace_id": null,
        "parent_session_id": null,
        "title": "导入会话",
        "status": "idle",
        "model": null,
        "thinking_depth": 2,
        "system_prompt": null,
        "config": {},
        "token_usage": {},
        "created_at": 10,
        "updated_at": 10,
        "closed_at": null,
    })
}

fn message_row(id: &str, seq: i64, client_msg_id: Option<&str>) -> Value {
    json!({
        "v": 1,
        "kind": "message",
        "session_id": SESSION_ID,
        "id": id,
        "role": "user",
        "content": "hello",
        "seq": seq,
        "ts": 11,
        "client_msg_id": client_msg_id,
    })
}

fn event_row(id: &str, seq: i64, event_type: &str) -> Value {
    json!({
        "v": 1,
        "kind": "event",
        "id": id,
        "session_id": SESSION_ID,
        "run_id": null,
        "runtime_id": "mock",
        "seq": seq,
        "ts": 12,
        "type": event_type,
        "payload": {"k": "v"},
    })
}

fn make_job(
    data_dir: &Path,
    job_id: &str,
    source: &Path,
    mode: ImportMode,
    status: ImportJobStatus,
) -> ImportJob {
    let job = ImportJob {
        v: 1,
        job_id: job_id.to_owned(),
        kind: "session".to_owned(),
        status,
        mode,
        dry_run: false,
        source_path: source.to_string_lossy().to_string(),
        source_bytes: std::fs::metadata(source).map(|m| m.len()).unwrap_or(0),
        source_sha256: None,
        progress: Default::default(),
        summary: None,
        warnings: Vec::new(),
        warnings_truncated: false,
        error: None,
        created_at: 1,
        updated_at: 1,
    };
    std::fs::create_dir_all(job_path(data_dir, job_id).parent().expect("job dir"))
        .expect("job dir");
    write_job(data_dir, &job).expect("写 job");
    write_pending(data_dir, job_id, 1).expect("写 pending");
    job
}

fn table_count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
        .expect("计数")
}

#[test]
fn validate_apply_remaps_ids_and_preserves_seqs_and_fields() {
    let dir = temp_dir("apply");
    let db = dir.path().join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);
    let verification = verify_archive_file(&report.path).expect("校验导出");
    assert!(verification.matches);

    let manifest = precheck_source(&report.path, &SESSION_IMPORT_SOURCES).expect("预检");
    assert_eq!(manifest.source, "session-export");

    let mut progress_calls = 0usize;
    let validation = validate_source(
        &report.path,
        &SESSION_IMPORT_SOURCES,
        &mut |_bytes, _rows| progress_calls += 1,
        &|| false,
    )
    .expect("校验");
    assert!(progress_calls > 0, "必须报告进度");
    assert_eq!(validation.summary.sessions, 1);
    assert_eq!(validation.summary.messages, 2);
    assert_eq!(validation.summary.events, 2);

    let work = work_db_path(dir.path(), "01J00000000000000000000J01");
    create_staging_snapshot(Some(&db), &work).expect("快照");
    let apply = apply_source_to_staging(
        &report.path,
        &work,
        &SESSION_IMPORT_SOURCES,
        ImportMode::Remap,
        1_700_000_001_000,
        &mut |_bytes, _rows| {},
        &|| false,
    )
    .expect("staging 应用");
    assert_eq!(apply.summary.sessions, 1);
    assert_eq!(apply.summary.messages, 2);
    assert_eq!(apply.summary.events, 2);

    mark_staging(
        &work,
        "01J00000000000000000000J01",
        1_700_000_001_000,
        json!({"source": "session-export"}),
    )
    .expect("marker");
    let verification = verify_staging(&work).expect("staging 验收");
    assert_eq!(verification.foreign_key_violations, 0);
    assert!(has_marker(&work, "01J00000000000000000000J01"));

    let connection = Connection::open(&work).expect("staging 连接");
    assert_eq!(table_count(&connection, "sessions"), 2, "源 + 导入副本");
    assert_eq!(table_count(&connection, "messages"), 4);
    assert_eq!(table_count(&connection, "events"), 4);
    // session-export 不包含 run/audit 行（仅全库归档导出 runs），故目标库保留原 1 行；
    // audit_log 另有 staging 完成标记（import.completed）。
    assert_eq!(table_count(&connection, "runs"), 1);
    assert_eq!(table_count(&connection, "audit_log"), 2);
    let imported_id: String = connection
        .query_row(
            "SELECT id FROM sessions WHERE id <> ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("导入会话");
    assert_ne!(imported_id, SESSION_ID);
    let title: String = connection
        .query_row(
            "SELECT title FROM sessions WHERE id = ?1",
            [&imported_id],
            |row| row.get(0),
        )
        .expect("标题");
    assert_eq!(title, "源会话");
    let seqs: Vec<i64> = connection
        .prepare("SELECT seq FROM messages WHERE session_id = ?1 ORDER BY seq")
        .expect("prepare")
        .query_map([&imported_id], |row| row.get(0))
        .expect("query")
        .map(Result::unwrap)
        .collect();
    assert_eq!(seqs, vec![1, 2], "消息 seq 按会话重排");
    let event_seqs: Vec<i64> = connection
        .prepare("SELECT seq FROM events WHERE session_id = ?1 ORDER BY seq")
        .expect("prepare")
        .query_map([&imported_id], |row| row.get(0))
        .expect("query")
        .map(Result::unwrap)
        .collect();
    assert_eq!(event_seqs, vec![1, 2], "事件 seq 按会话重排");
}

#[test]
fn boot_apply_switches_atomically_and_is_idempotent() {
    let dir = temp_dir("boot");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db = data_dir.join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);
    let job_id = "01J00000000000000000000J02";
    make_job(
        &data_dir,
        job_id,
        &report.path,
        ImportMode::Remap,
        ImportJobStatus::AwaitingRestart,
    );

    let outcome = boot_apply_pending_import(&data_dir, 1_700_000_002_000, |_| Ok(u64::MAX))
        .expect("启动导入")
        .expect("有待处理");
    assert!(!outcome.already_applied);
    assert!(!pending_exists(&data_dir), "切换后必须删除 pending");
    let connection = Connection::open(&db).expect("库连接");
    assert_eq!(table_count(&connection, "sessions"), 2);
    assert!(has_marker(&db, job_id));
    let completed_audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'import.completed' AND resource = ?1",
            [job_id],
            |row| row.get(0),
        )
        .expect("审计");
    assert_eq!(completed_audits, 1, "完成审计必须落库且幂等");
    drop(connection);
    let pre_import: Vec<String> = std::fs::read_dir(&data_dir)
        .expect("data dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".pre-import-"))
        .collect();
    assert!(!pre_import.is_empty(), "必须保留 .pre-import 现场：{pre_import:?}");

    // 第二次启动：pending 已清理 → 无待处理；标记仍在（幂等）。
    let again = boot_apply_pending_import(&data_dir, 1_700_000_002_100, |_| Ok(u64::MAX))
        .expect("第二次启动");
    assert!(again.is_none());
    assert!(has_marker(&db, job_id));
}

#[test]
fn preserve_conflict_is_structured_and_does_not_change_live_db() {
    let dir = temp_dir("preserve");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db = data_dir.join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);
    let job_id = "01J00000000000000000000J03";
    make_job(
        &data_dir,
        job_id,
        &report.path,
        ImportMode::Preserve,
        ImportJobStatus::AwaitingRestart,
    );
    let failure = boot_apply_pending_import(&data_dir, 1_700_000_003_000, |_| Ok(u64::MAX))
        .expect_err("preserve 冲突必须失败");
    assert_eq!(failure.kind, ImportFailureKind::Conflict);
    assert!(failure
        .details
        .as_ref()
        .and_then(|details| details.get("conflicts"))
        .is_some());
    let connection = Connection::open(&db).expect("库连接");
    assert_eq!(table_count(&connection, "sessions"), 1, "既有库不变");
    assert!(!pending_exists(&data_dir));
    let job: ImportJob = serde_json::from_str(
        &std::fs::read_to_string(job_path(&data_dir, job_id)).expect("job"),
    )
    .expect("job json");
    assert_eq!(job.status, ImportJobStatus::Failed);
    assert_eq!(
        job.error.as_ref().map(|error| error.code.as_str()),
        Some("import_conflict")
    );
}

#[test]
fn checksum_mismatch_and_tampering_are_rejected() {
    let dir = temp_dir("checksum");
    let db = dir.path().join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);
    let text = std::fs::read_to_string(&report.path).expect("读");
    let mut lines = text.lines();
    let manifest = lines.next().expect("manifest");
    let mut manifest: Value = serde_json::from_str(manifest).expect("manifest json");
    manifest["sha256"] = json!("b".repeat(64));
    let payload: Vec<&str> = lines.collect();
    let tampered = format!(
        "{}\n{}\n",
        serde_json::to_string(&manifest).expect("manifest"),
        payload.join("\n")
    );
    std::fs::write(&report.path, tampered).expect("篡改");
    let failure = validate_source(&report.path, &SESSION_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect_err("行数/哈希不一致必须失败");
    assert_eq!(failure.kind, ImportFailureKind::ChecksumMismatch);
}

#[test]
fn invalid_kind_unknown_event_secret_field_and_oversized_line_are_structured() {
    let dir = temp_dir("invalid");
    let source = dir.path().join("invalid.jsonl");

    // 未知 kind → 整文件拒绝。
    write_jsonl(
        &source,
        "session-export",
        Some(SESSION_ID),
        vec![json!({"v":1,"kind":"future-row","id":"x"})],
    );
    let failure = validate_source(&source, &SESSION_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect_err("未知 kind");
    assert_eq!(failure.kind, ImportFailureKind::InvalidFormat);

    // 密钥字段 → 拒绝（D10）。
    let mut session = session_row(SESSION_ID);
    session["api_key"] = json!("dummy-not-a-key");
    write_jsonl(
        &source,
        "session-export",
        Some(SESSION_ID),
        vec![session],
    );
    let failure = validate_source(&source, &SESSION_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect_err("密钥字段");
    assert_eq!(failure.kind, ImportFailureKind::InvalidFormat);
    assert!(failure.message.contains("api_key"));

    // 未知事件类型 → 跳过 + 警告（不执行/不广播）。
    write_jsonl(
        &source,
        "session-export",
        Some(SESSION_ID),
        vec![
            session_row(SESSION_ID),
            event_row(EVENT_1, 1, "future.event"),
        ],
    );
    let validation = validate_source(&source, &SESSION_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect("未知事件类型应跳过");
    assert_eq!(validation.summary.events, 0);
    assert_eq!(validation.summary.skipped, 1);
    assert!(validation
        .warnings
        .iter()
        .any(|warning| warning.code == "unknown_event_type"));

    // 超长行 → too_large（流式上限）。
    let oversized = dir.path().join("oversized.jsonl");
    let mut text = String::new();
    let manifest = manifest_line("session-export", Some(SESSION_ID), &[]);
    text.push_str(&serde_json::to_string(&manifest).expect("manifest"));
    text.push('\n');
    text.push_str(&"x".repeat(2 * 1024 * 1024 + 16));
    text.push('\n');
    std::fs::write(&oversized, text).expect("写超长行");
    let failure = validate_source(&oversized, &SESSION_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect_err("超长行");
    assert_eq!(failure.kind, ImportFailureKind::TooLarge);
}

#[test]
fn client_msg_id_dedupes_and_apply_is_isolated() {
    let dir = temp_dir("dedupe");
    let db = dir.path().join("aether.db");
    seed_database(&db);
    let source = dir.path().join("dup.jsonl");
    write_jsonl(
        &source,
        "session-export",
        Some(SESSION_ID),
        vec![
            session_row(SESSION_ID),
            message_row("01J00000000000000000000D2", 1, Some("dup-client")),
            message_row("01J00000000000000000000D3", 2, Some("dup-client")),
        ],
    );
    let work = dir.path().join("work.db");
    create_staging_snapshot(Some(&db), &work).expect("快照");
    let report = apply_source_to_staging(
        &source,
        &work,
        &SESSION_IMPORT_SOURCES,
        ImportMode::Remap,
        1,
        &mut |_, _| {},
        &|| false,
    )
    .expect("apply");
    assert_eq!(report.summary.messages, 1);
    assert_eq!(report.summary.duplicates, 1);
    let connection = Connection::open(&work).expect("staging");
    assert_eq!(table_count(&connection, "messages"), 3, "源 2 + 导入 1");
}

#[test]
fn cancellation_stops_at_boundary_and_interrupted_job_is_not_resumed() {
    let dir = temp_dir("cancel");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db = data_dir.join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);

    let cancelled = AtomicBool::new(true);
    let failure = validate_source(
        &report.path,
        &SESSION_IMPORT_SOURCES,
        &mut |_, _| {},
        &|| cancelled.load(Ordering::SeqCst),
    )
    .expect_err("取消");
    assert_eq!(failure.kind, ImportFailureKind::Cancelled);

    // 模拟 kill -9：job 状态 applying + pending；启动不得自动续跑。
    let job_id = "01J00000000000000000000J04";
    make_job(
        &data_dir,
        job_id,
        &report.path,
        ImportMode::Remap,
        ImportJobStatus::Applying,
    );
    let failure = boot_apply_pending_import(&data_dir, 1_700_000_004_000, |_| Ok(u64::MAX))
        .expect_err("中断作业");
    assert_eq!(failure.kind, ImportFailureKind::Internal);
    let connection = Connection::open(&db).expect("库");
    assert_eq!(table_count(&connection, "sessions"), 1, "既有库不变");
    assert!(!pending_exists(&data_dir));
    let job: ImportJob = serde_json::from_str(
        &std::fs::read_to_string(job_path(&data_dir, job_id)).expect("job"),
    )
    .expect("job json");
    assert_eq!(job.status, ImportJobStatus::Failed);
}

#[test]
fn interrupted_validating_job_is_reconciled_without_pending() {
    let dir = temp_dir("reconcile");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db = data_dir.join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = export_session(&db, &exports);
    let job_id = "01J00000000000000000000J08";
    make_job(
        &data_dir,
        job_id,
        &report.path,
        ImportMode::Remap,
        ImportJobStatus::Validating,
    );
    remove_pending(&data_dir).expect("移除 pending");
    let repaired = reconcile_interrupted_imports(&data_dir, 1_700_000_008_000).expect("reconcile");
    assert_eq!(repaired, 1);
    let job: ImportJob = serde_json::from_str(
        &std::fs::read_to_string(job_path(&data_dir, job_id)).expect("job"),
    )
    .expect("job json");
    assert_eq!(job.status, ImportJobStatus::Failed);
    assert_eq!(
        job.error.as_ref().map(|error| error.code.as_str()),
        Some("import_interrupted")
    );
    let connection = Connection::open(&db).expect("库");
    assert_eq!(table_count(&connection, "sessions"), 1, "既有库不变");
}

#[test]
fn stale_staging_and_pre_import_artifacts_are_cleaned() {
    let dir = temp_dir("cleanup");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let stale_job = "01J00000000000000000000J05";
    let job_dir = job_path(&data_dir, stale_job)
        .parent()
        .expect("job dir")
        .to_path_buf();
    std::fs::create_dir_all(&job_dir).expect("job dir");
    std::fs::write(work_db_path(&data_dir, stale_job), b"stale").expect("work");
    std::fs::write(job_path(&data_dir, stale_job), b"{}").expect("job");
    std::fs::write(data_dir.join("aether.db.pre-import-1"), b"old").expect("pre");
    // at_ms 超出 24h 窗口 → 全部清理（基于当前时间，避免历史时间戳误判）。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis() as i64;
    let removed = cleanup_imports(&data_dir, now + 3 * 24 * 3_600_000).expect("清理");
    assert!(removed >= 2, "至少清理 staging + .pre-import：{removed}");
    assert!(!job_dir.exists());
    assert!(!data_dir.join("aether.db.pre-import-1").exists());
}

#[test]
fn roundtrip_export_import_export_preserves_rows_and_counts() {
    let dir = temp_dir("roundtrip");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db = data_dir.join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let first = export_session(&db, &exports);
    let job_id = "01J00000000000000000000J06";
    make_job(
        &data_dir,
        job_id,
        &first.path,
        ImportMode::Remap,
        ImportJobStatus::AwaitingRestart,
    );
    boot_apply_pending_import(&data_dir, 1_700_000_005_000, |_| Ok(u64::MAX))
        .expect("导入")
        .expect("有待处理");
    let connection = Connection::open(&db).expect("库");
    let imported_id: String = connection
        .query_row(
            "SELECT id FROM sessions WHERE id <> ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("导入会话");
    drop(connection);
    let second = export_session_archive(
        &Connection::open(&db).expect("只读"),
        &exports,
        &imported_id,
        1_700_000_006_000,
        &identity,
    )
    .expect("再导出")
    .expect("导入会话存在");
    assert_eq!(first.counts.sessions, second.counts.sessions);
    assert_eq!(first.counts.messages, second.counts.messages);
    assert_eq!(first.counts.events, second.counts.events);
    assert_eq!(first.counts.runs, second.counts.runs);
    let verification = verify_archive_file(&second.path).expect("再导出校验");
    assert!(verification.matches);
    let connection = Connection::open(&db).expect("库");
    let summary = ImportSummary {
        sessions: table_count(&connection, "sessions") as u64,
        messages: table_count(&connection, "messages") as u64,
        ..ImportSummary::default()
    };
    assert_eq!(summary.sessions, 2);
    assert_eq!(summary.messages, 4);
}

#[test]
#[ignore = "≥1GB 大库样本；AETHER_M10_04_BIG_MIB 驱动"]
fn big_import_sample_is_streamed() {
    let mib: u64 = std::env::var("AETHER_M10_04_BIG_MIB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);
    let dir = temp_dir("big");
    let source = dir.path().join("big.jsonl");
    let target_bytes = mib * 1024 * 1024;
    let payload = "x".repeat(512);
    let mut rows = Vec::new();
    let mut approx = 0u64;
    while approx < target_bytes {
        let value = json!({
            "v": 1,
            "kind": "event",
            "id": format!("big-{:08}", rows.len()),
            "session_id": SESSION_ID,
            "run_id": null,
            "runtime_id": "mock",
            "seq": rows.len() as i64 + 1,
            "ts": 1,
            "type": "log",
            "payload": {"text": payload},
        });
        approx = approx.saturating_add(
            serde_json::to_string(&value)
                .expect("样本行序列化")
                .len()
                .saturating_add(1) as u64,
        );
        rows.push(value);
    }
    write_jsonl(&source, "archive-export", None, rows);
    let bytes = std::fs::metadata(&source).expect("metadata").len();
    assert!(bytes >= target_bytes, "样本应 ≥ {mib} MiB，实际 {bytes}");
    let work = dir.path().join("work.db");
    create_staging_snapshot(None, &work).expect("staging 空库");
    let begin = std::time::Instant::now();
    let validation = validate_source(&source, &ARCHIVE_IMPORT_SOURCES, &mut |_, _| {}, &|| false)
        .expect("大文件校验");
    assert!(validation.summary.events > 0);
    let apply = apply_source_to_staging(
        &source,
        &work,
        &ARCHIVE_IMPORT_SOURCES,
        ImportMode::Remap,
        1,
        &mut |_, _| {},
        &|| false,
    );
    // 该样本没有 session 行，事件允许保留原始 session id；若实现拒绝则至少证明流式读取有界。
    match apply {
        Ok(report) => assert!(report.summary.events > 0),
        Err(error) => assert!(
            matches!(
                error.kind,
                ImportFailureKind::InvalidFormat | ImportFailureKind::Internal
            ),
            "大文件失败必须是结构化错误：{error}"
        ),
    }
    println!(
        "big import: bytes={bytes} rows={} validation_ms={} total_ms={}",
        validation.rows,
        begin.elapsed().as_millis(),
        begin.elapsed().as_millis()
    );
}

#[test]
fn source_manifest_and_full_archive_are_supported() {
    let dir = temp_dir("full");
    let db = dir.path().join("aether.db");
    let exports = dir.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");
    seed_database(&db);
    let report = {
        let connection = Connection::open(&db).expect("连接");
        export_full_archive(&connection, &exports, 1_700_000_007_000, &identity).expect("全库导出")
    };
    let manifest = precheck_source(&report.path, &ARCHIVE_IMPORT_SOURCES).expect("预检");
    assert_eq!(manifest.source, "archive-export");
    let work = dir.path().join("work.db");
    create_staging_snapshot(Some(&db), &work).expect("快照");
    let apply = apply_source_to_staging(
        &report.path,
        &work,
        &ARCHIVE_IMPORT_SOURCES,
        ImportMode::Remap,
        1,
        &mut |_, _| {},
        &|| false,
    )
    .expect("全库导入");
    assert_eq!(apply.summary.sessions, 1);
    assert_eq!(apply.summary.runs, 1);
    let connection = Connection::open(&work).expect("staging");
    assert_eq!(table_count(&connection, "sessions"), 2);
    assert_eq!(table_count(&connection, "runs"), 2);
    let imported_id: String = connection
        .query_row(
            "SELECT id FROM sessions WHERE id <> ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("导入会话");
    let run_input: Option<String> = connection
        .query_row(
            "SELECT input_message_id FROM runs WHERE session_id = ?1",
            [&imported_id],
            |row| row.get(0),
        )
        .expect("导入 run");
    assert!(
        run_input.is_some_and(|id| id != MESSAGE_USER),
        "run.input_message_id 必须映射到新消息 id"
    );
    let event_run: Option<String> = connection
        .query_row(
            "SELECT run_id FROM events WHERE session_id = ?1 AND run_id IS NOT NULL LIMIT 1",
            [&imported_id],
            |row| row.get(0),
        )
        .optional()
        .expect("event run");
    // 全库归档的事件 run_id 必须映射（若源事件未带 run_id 则为 None）。
    if let Some(run_id) = event_run {
        let exists: i64 = connection
            .query_row("SELECT COUNT(*) FROM runs WHERE id = ?1", [&run_id], |row| {
                row.get(0)
            })
            .expect("run exists");
        assert_eq!(exists, 1, "事件 run_id 不得悬挂");
    }
}

#[test]
fn marker_is_scoped_and_missing_marker_is_false() {
    let dir = temp_dir("marker");
    let db = dir.path().join("aether.db");
    seed_database(&db);
    assert!(!has_marker(&db, "01J00000000000000000000J07"));
    let connection = Connection::open(&db).expect("连接");
    connection
        .execute(
            "INSERT INTO audit_log \
             (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
             VALUES ('marker-1', NULL, NULL, 'user', 'import.completed', ?1, '{}', 'success', 1)",
            ["01J00000000000000000000J07"],
        )
        .expect("marker");
    assert!(has_marker(&db, "01J00000000000000000000J07"));
    assert!(!has_marker(&db, "01J00000000000000000000J08"));
}

#[test]
fn manifest_struct_is_public_and_roundtrips() {
    let manifest = ImportManifest {
        v: 1,
        kind: "manifest".to_owned(),
        created_at: Some(1),
        source: "session-export".to_owned(),
        session_id: Some(SESSION_ID.to_owned()),
        rows: 0,
        sha256: "a".repeat(64),
        exported_by: None,
        app_version: None,
    };
    let json = serde_json::to_string(&manifest).expect("json");
    let decoded: ImportManifest = serde_json::from_str(&json).expect("decode");
    assert_eq!(decoded, manifest);
    let _ = PathBuf::new();
}
