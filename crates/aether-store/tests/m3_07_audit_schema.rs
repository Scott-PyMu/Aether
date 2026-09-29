//! M3-07 DoD3：`audit_log` 最小集字段 schema 断言（设计 D9 / SE-03 / 附录 C）。
//!
//! 断言 `actor` / `resource` / `result` / `ts` 四字段齐备（名称、类型、NOT NULL 约束），
//! 且 `audit_log` 仅追加语义（无触发器改写；应用层 UPDATE/DELETE 路径由
//! `scripts/test/m3-07/verify-m3-07.mjs` 静态审计）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use rusqlite::Connection;

/// `PRAGMA table_info` 行：名称 / 类型 / NOT NULL / 默认值 / 主键位。
struct Column {
    name: String,
    declared_type: String,
    notnull: bool,
}

fn columns(conn: &Connection, table: &str) -> Vec<Column> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(Column {
                name: row.get::<_, String>(1)?,
                declared_type: row.get::<_, String>(2)?,
                notnull: row.get::<_, i64>(3)? != 0,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn audit_log_schema_has_minimal_audit_fields() {
    let (_dir, store) = common::open_temp_store("m3-07-schema");
    let columns = columns(store.connection(), "audit_log");
    let find = |name: &str| {
        columns
            .iter()
            .find(|column| column.name == name)
            .unwrap_or_else(|| panic!("audit_log 缺少字段 {name}"))
    };

    // 附录 C DDL：actor/ts NOT NULL；resource/result 可空但必须存在。
    let actor = find("actor");
    assert_eq!(actor.declared_type, "TEXT");
    assert!(actor.notnull, "actor 必须 NOT NULL");
    let resource = find("resource");
    assert_eq!(resource.declared_type, "TEXT");
    let result = find("result");
    assert_eq!(result.declared_type, "TEXT");
    let ts = find("ts");
    assert_eq!(ts.declared_type, "INTEGER");
    assert!(ts.notnull, "ts 必须 NOT NULL");

    // 最小集其余字段（归属/动作/明细）。
    for name in ["id", "session_id", "runtime_id", "action", "detail"] {
        find(name);
    }
}

#[test]
fn audit_log_has_no_rewrite_triggers() {
    let (_dir, store) = common::open_temp_store("m3-07-triggers");
    let count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'audit_log'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "audit_log 不得有触发器（仅追加语义）");
}

#[test]
fn audit_log_accepts_and_reads_back_minimal_fields() {
    let (_dir, store) = common::open_temp_store("m3-07-roundtrip");
    let conn = store.connection();
    conn.execute(
        "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
         VALUES ('01J000000000000000000000A1', NULL, 'mock', 'system', 'runtime.status_changed', \
                 'runtime:mock', '{\"from\":\"cold\",\"to\":\"ready\"}', 'cold→ready', 7)",
        [],
    )
    .unwrap();
    let (actor, resource, result, ts): (String, Option<String>, Option<String>, i64) = conn
        .query_row(
            "SELECT actor, resource, result, ts FROM audit_log WHERE id = '01J000000000000000000000A1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(actor, "system");
    assert_eq!(resource.as_deref(), Some("runtime:mock"));
    assert_eq!(result.as_deref(), Some("cold→ready"));
    assert_eq!(ts, 7);
}
