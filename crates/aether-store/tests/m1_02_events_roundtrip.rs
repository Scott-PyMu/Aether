//! M1-02 DoD1/DoD4：信封字段 ↔ `events` 表列的一一对应与真实 SQLite 入库往返。
//!
//! 与 `aether-core` 的静态契约测试互补：本测试用 `rusqlite`（bundled SQLite）执行
//! 迁移 0001，真实建表、真实 INSERT/SELECT，验证「序列化 → 入库 → 反序列化等价」。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use aether_core::{
    EventEnvelope, EventId, EventPayload, EventType, RunId, RuntimeId, SessionId, ENVELOPE_FIELDS,
};
use rusqlite::{params, Connection};
use serde_json::Value;

const MIGRATION_SQL: &str = include_str!("../../../migrations/0001_init.sql");

fn open_migrated() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(MIGRATION_SQL).unwrap();
    conn
}

fn table_info(conn: &Connection, table: &str) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn column_names(conn: &Connection, table: &str) -> BTreeSet<String> {
    table_info(conn, table)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn column_types(conn: &Connection, table: &str) -> BTreeMap<String, String> {
    table_info(conn, table).into_iter().collect()
}

fn json_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("信封序列化必须为对象")
        .keys()
        .cloned()
        .collect()
}

/// 附录 B 全部 23 种类型的合法 payload 样本 + 信封 `run_id`。
fn envelope_samples() -> Vec<(EventType, &'static str, Option<&'static str>)> {
    use EventType::*;
    vec![
        (
            SessionCreated,
            r#"{"summary":{"id":"sess-1","runtime_id":"mock","workspace_id":null,"title":"t","status":"idle","model":null,"created_at":1,"updated_at":2}}"#,
            Some("run-1"),
        ),
        (
            SessionUpdated,
            r#"{"session_id":"sess-1","changed_fields":["title"]}"#,
            None,
        ),
        (
            SessionStatusChanged,
            r#"{"session_id":"sess-1","from":"idle","to":"running"}"#,
            Some("run-1"),
        ),
        (
            SessionClosed,
            r#"{"session_id":"sess-1","closed_at":3}"#,
            None,
        ),
        (RunStarted, r#"{"run_id":"run-1"}"#, Some("run-1")),
        (
            RunCompleted,
            r#"{"run_id":"run-1","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#,
            Some("run-1"),
        ),
        (
            RunFailed,
            r#"{"run_id":"run-1","error":{"code":"E1","message":"boom","recoverable":false}}"#,
            Some("run-1"),
        ),
        (
            RunCancelled,
            r#"{"run_id":"run-1","reason":"user"}"#,
            Some("run-1"),
        ),
        (
            MessageDelta,
            r#"{"message_id":"msg-1","text":"hello"}"#,
            Some("run-1"),
        ),
        (
            MessageCompleted,
            r#"{"message":{"id":"msg-1","session_id":"sess-1","run_id":"run-1","role":"assistant","content":"done","created_at":4},"usage":null}"#,
            Some("run-1"),
        ),
        (
            ToolCallStarted,
            r#"{"tool_call_id":"tc-1","tool_name":"read_file","args":{"path":"a.txt"}}"#,
            Some("run-1"),
        ),
        (
            ToolCallCompleted,
            r#"{"tool_call_id":"tc-1","tool_name":"read_file","duration_ms":12}"#,
            Some("run-1"),
        ),
        (
            ToolCallFailed,
            r#"{"tool_call_id":"tc-1","tool_name":"read_file","duration_ms":12,"error":{"code":"E1","message":"boom","recoverable":true}}"#,
            Some("run-1"),
        ),
        (
            PermissionRequested,
            r#"{"request_id":"perm-1","resource":"fs.read","action":"read","target":"a.txt"}"#,
            Some("run-1"),
        ),
        (
            PermissionResolved,
            r#"{"request_id":"perm-1","decision":"allow","scope":"once"}"#,
            Some("run-1"),
        ),
        (
            RuntimeStatusChanged,
            r#"{"runtime_id":"mock","from":"starting","to":"ready","reason":null}"#,
            None,
        ),
        (
            Usage,
            r#"{"tokens":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#,
            Some("run-1"),
        ),
        (Log, r#"{"level":"info","message":"started"}"#, None),
        (
            Error,
            r#"{"code":"E1","message":"boom","recoverable":true}"#,
            None,
        ),
        (
            MessageReasoningDelta,
            r#"{"message_id":"msg-1","text":"think"}"#,
            Some("run-1"),
        ),
        (
            SubagentSpawned,
            r#"{"child_session_id":"sess-child","mode":"explore"}"#,
            Some("run-1"),
        ),
        (
            SubagentCompleted,
            r#"{"child_session_id":"sess-child","verdict":"ok"}"#,
            Some("run-1"),
        ),
        (
            Workflow,
            r#"{"workflow_run_id":"wfrun-1","node_id":null,"status":"running"}"#,
            Some("run-1"),
        ),
    ]
}

fn envelope_from_sample(
    index: usize,
    event_type: EventType,
    payload: &str,
    run_id: Option<&str>,
) -> EventEnvelope {
    let run_id_json = match run_id {
        Some(value) => format!("\"{value}\""),
        None => "null".to_string(),
    };
    let text = format!(
        r#"{{"v":1,"id":"01J0000000000000000000{index:02}","session_id":"sess-1","run_id":{run_id_json},"runtime_id":"mock","seq":{},"ts":1760000000000,"type":"{event_type}","payload":{payload}}}"#,
        100 + index
    );
    EventEnvelope::from_json_str(&text).unwrap()
}

struct StoredRow {
    id: String,
    session_id: String,
    run_id: Option<String>,
    runtime_id: String,
    seq: i64,
    event_type: String,
    payload: String,
    ts: i64,
    v: i64,
}

fn insert_envelope(conn: &Connection, envelope: &EventEnvelope) {
    conn.execute(
        "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            envelope.id.as_str(),
            envelope.session_id.as_str(),
            envelope.run_id.as_ref().map(RunId::as_str),
            envelope.runtime_id.as_str(),
            i64::try_from(envelope.seq).unwrap(),
            envelope.event_type().as_str(),
            envelope.payload.to_value().unwrap().to_string(),
            envelope.ts,
            envelope.v,
        ],
    )
    .unwrap();
}

fn select_row(conn: &Connection, id: &str) -> StoredRow {
    conn.query_row(
        "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v FROM events WHERE id = ?1",
        [id],
        |row| {
            Ok(StoredRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                run_id: row.get(2)?,
                runtime_id: row.get(3)?,
                seq: row.get(4)?,
                event_type: row.get(5)?,
                payload: row.get(6)?,
                ts: row.get(7)?,
                v: row.get(8)?,
            })
        },
    )
    .unwrap()
}

// ===== DoD4：events 表含 v/runtime_id 列（真实 SQLite 断言） =====

#[test]
fn events_table_has_v_and_runtime_id_columns_with_expected_types() {
    let conn = open_migrated();
    let types = column_types(&conn, "events");

    assert_eq!(types.get("v").map(String::as_str), Some("INTEGER"));
    assert_eq!(types.get("runtime_id").map(String::as_str), Some("TEXT"));
    assert_eq!(types.get("seq").map(String::as_str), Some("INTEGER"));
    assert_eq!(types.get("payload").map(String::as_str), Some("TEXT"));
    assert_eq!(types.len(), 9, "events 表列数应为 9");
}

// ===== DoD1：信封字段 ↔ events 表列（双向，真实 DB 元数据） =====

#[test]
fn events_columns_and_envelope_fields_are_bidirectionally_equal_in_sqlite() {
    let conn = open_migrated();
    let columns = column_names(&conn, "events");
    let fields: BTreeSet<String> = ENVELOPE_FIELDS.iter().map(|f| f.to_string()).collect();
    assert_eq!(columns, fields, "events 列集合必须等于信封字段集合");

    let envelope = envelope_from_sample(
        0,
        EventType::MessageDelta,
        r#"{"message_id":"msg-1","text":"hi"}"#,
        Some("run-1"),
    );
    let keys = json_keys(&envelope.to_json_value().unwrap());
    assert_eq!(keys, columns, "信封序列化键集合必须等于 events 列集合");

    for column in &columns {
        assert!(fields.contains(column), "表列 {column} 不在信封字段中");
    }
    for field in &fields {
        assert!(columns.contains(field), "信封字段 {field} 不在表列中");
    }
}

// ===== DoD1：序列化 → 入库 → 反序列化等价（全类型） =====

#[test]
fn all_event_types_roundtrip_through_sqlite_losslessly() {
    let conn = open_migrated();
    let samples = envelope_samples();
    assert_eq!(
        samples.len(),
        EventType::ALL.len(),
        "样本必须覆盖附录 B 全部类型"
    );
    assert_eq!(
        samples
            .iter()
            .map(|(event_type, _, _)| *event_type)
            .collect::<BTreeSet<_>>(),
        EventType::ALL.into_iter().collect::<BTreeSet<_>>(),
        "样本类型集合必须等于 EventType::ALL"
    );

    for (index, (event_type, payload, run_id)) in samples.into_iter().enumerate() {
        let envelope = envelope_from_sample(index, event_type, payload, run_id);
        insert_envelope(&conn, &envelope);

        let row = select_row(&conn, envelope.id.as_str());
        assert_eq!(row.v, i64::from(envelope.v), "{event_type}: v 列不一致");
        assert_eq!(row.id, envelope.id.as_str());
        assert_eq!(row.session_id, envelope.session_id.as_str());
        assert_eq!(
            row.run_id.as_deref(),
            envelope.run_id.as_ref().map(RunId::as_str),
            "{event_type}: run_id 列不一致"
        );
        assert_eq!(row.runtime_id, envelope.runtime_id.as_str());
        assert_eq!(row.seq, i64::try_from(envelope.seq).unwrap());
        assert_eq!(row.event_type, event_type.as_str());
        assert_eq!(row.ts, envelope.ts);
        assert_eq!(
            serde_json::from_str::<Value>(&row.payload).unwrap(),
            envelope.payload.to_value().unwrap(),
            "{event_type}: payload 列不等价"
        );

        let reconstructed = EventEnvelope {
            v: u32::try_from(row.v).unwrap(),
            id: EventId::new(row.id).unwrap(),
            session_id: SessionId::new(row.session_id).unwrap(),
            run_id: row.run_id.map(|value| RunId::new(value).unwrap()),
            runtime_id: RuntimeId::new(row.runtime_id).unwrap(),
            seq: u64::try_from(row.seq).unwrap(),
            ts: row.ts,
            payload: EventPayload::parse(event_type, serde_json::from_str(&row.payload).unwrap())
                .unwrap(),
        };
        assert_eq!(reconstructed, envelope, "{event_type}: 入库往返不等价");
    }
}

#[test]
fn null_run_id_is_stored_and_read_back_as_none() {
    let conn = open_migrated();
    let envelope = envelope_from_sample(
        1,
        EventType::SessionUpdated,
        r#"{"session_id":"sess-1","changed_fields":["title"]}"#,
        None,
    );
    assert_eq!(envelope.run_id, None);
    insert_envelope(&conn, &envelope);

    let row = select_row(&conn, envelope.id.as_str());
    assert_eq!(row.run_id, None);
    let value: Value = serde_json::from_str(&row.payload).unwrap();
    assert_eq!(
        EventPayload::parse(EventType::SessionUpdated, value).unwrap(),
        envelope.payload
    );
}

#[test]
fn events_table_persists_one_row_per_envelope() {
    let conn = open_migrated();
    for (index, (event_type, payload, run_id)) in envelope_samples().into_iter().enumerate() {
        let envelope = envelope_from_sample(index, event_type, payload, run_id);
        insert_envelope(&conn, &envelope);
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, i64::try_from(EventType::ALL.len()).unwrap());

    let json_payloads: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE json_valid(payload) = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        json_payloads, count,
        "全部 payload 列必须是合法 JSON（附录 C：payload TEXT 存 JSON）"
    );

    let distinct_seq: i64 = conn
        .query_row("SELECT COUNT(DISTINCT seq) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(distinct_seq, count, "seq 必须唯一（D4 单 sequencer 口径）");
}
