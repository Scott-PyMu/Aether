//! M1-03 DoD3：附录 C 全部表、外键、索引与 DDL 约束的行为断言。
//!
//! 结构逐项比对（列/类型/默认值/外键/索引 ↔ 附录 C）由
//! `scripts/test/m1-03/verify-m1-03.mjs` 对 `schema_dump` 输出执行；
//! 本文件覆盖 DDL 语义：表清单、索引、外键强制、CHECK 枚举、默认值。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_store::StoreError;
use rusqlite::Connection;

/// 附录 C 全部表（含 schema_migrations，共 17 张）。
const APPENDIX_C_TABLES: &[&str] = &[
    "adapter_plugins",
    "audit_log",
    "backups",
    "events",
    "memories",
    "messages",
    "node_runs",
    "permissions",
    "runs",
    "runtimes",
    "schema_migrations",
    "sessions",
    "settings",
    "tasks",
    "workflow_runs",
    "workflows",
    "workspaces",
];

fn is_constraint_violation(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn table_names(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn user_indexes(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn index_columns(conn: &Connection, index: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA index_info({index})"))
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(2))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn foreign_keys(conn: &Connection, table: &str) -> Vec<(String, String, String, String)> {
    let mut statement = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(3)?, // from
                row.get::<_, String>(2)?, // referenced table
                row.get::<_, String>(4)?, // referenced column
                row.get::<_, String>(6)?, // on_delete
            ))
        })
        .unwrap();
    let mut keys: Vec<_> = rows.map(Result::unwrap).collect();
    keys.sort();
    keys
}

#[test]
fn appendix_c_all_tables_exist_after_migration() {
    let (_dir, store) = common::open_temp_store("schema-tables");
    let names = table_names(store.connection());
    let expected: Vec<String> = APPENDIX_C_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(names, expected, "表集合必须与附录 C 一致");
}

#[test]
fn appendix_c_explicit_indexes_exist_with_expected_columns() {
    let (_dir, store) = common::open_temp_store("schema-indexes");
    let conn = store.connection();

    let expected: &[(&str, &str, &[&str])] = &[
        ("idx_audit_ts", "audit_log", &["ts"]),
        ("idx_events_session_seq", "events", &["session_id", "seq"]),
        ("idx_events_type_ts", "events", &["type", "ts"]),
        (
            "idx_messages_session_seq",
            "messages",
            &["session_id", "seq"],
        ),
        ("idx_sessions_parent", "sessions", &["parent_session_id"]),
        (
            "idx_sessions_runtime",
            "sessions",
            &["runtime_id", "status"],
        ),
    ];

    let mut expected_names: Vec<String> = expected
        .iter()
        .map(|(name, _, _)| (*name).to_owned())
        .collect();
    expected_names.sort();
    assert_eq!(
        user_indexes(conn),
        expected_names,
        "不得增删附录 C 之外的索引"
    );

    for (name, table, columns) in expected {
        let actual_table: String = conn
            .query_row(
                "SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual_table, *table, "{name} 必须建在 {table} 上");
        let actual: Vec<String> = index_columns(conn, name);
        assert_eq!(
            actual,
            columns
                .iter()
                .map(|column| (*column).to_owned())
                .collect::<Vec<_>>(),
            "{name} 的列顺序必须与附录 C 一致"
        );
    }
}

#[test]
fn appendix_c_foreign_keys_exist() {
    let (_dir, store) = common::open_temp_store("schema-fks");
    let conn = store.connection();

    let expected: &[(&str, &str, &str, &str, &str)] = &[
        (
            "messages",
            "parent_message_id",
            "messages",
            "id",
            "NO ACTION",
        ),
        ("messages", "session_id", "sessions", "id", "CASCADE"),
        ("memories", "workspace_id", "workspaces", "id", "CASCADE"),
        (
            "node_runs",
            "workflow_run_id",
            "workflow_runs",
            "id",
            "CASCADE",
        ),
        ("permissions", "session_id", "sessions", "id", "CASCADE"),
        ("runs", "session_id", "sessions", "id", "CASCADE"),
        (
            "sessions",
            "parent_session_id",
            "sessions",
            "id",
            "SET NULL",
        ),
        ("sessions", "runtime_id", "runtimes", "id", "NO ACTION"),
        ("sessions", "workspace_id", "workspaces", "id", "NO ACTION"),
        ("tasks", "session_id", "sessions", "id", "CASCADE"),
        (
            "workflow_runs",
            "workflow_id",
            "workflows",
            "id",
            "NO ACTION",
        ),
    ];

    let mut all: Vec<(String, String, String, String, String)> = Vec::new();
    for table in APPENDIX_C_TABLES {
        for (from, target, to, on_delete) in foreign_keys(conn, table) {
            all.push(((*table).to_owned(), from, target, to, on_delete));
        }
    }
    all.sort();

    let mut expected_sorted: Vec<(String, String, String, String, String)> = expected
        .iter()
        .map(|(table, from, target, to, on_delete)| {
            (
                (*table).to_owned(),
                (*from).to_owned(),
                (*target).to_owned(),
                (*to).to_owned(),
                (*on_delete).to_owned(),
            )
        })
        .collect();
    expected_sorted.sort();
    assert_eq!(
        all, expected_sorted,
        "外键集合（含 ON DELETE）必须与附录 C 一致"
    );
}

#[test]
fn foreign_keys_are_enforced_and_cascade() {
    let (_dir, store) = common::open_temp_store("schema-fk-behavior");

    // 非法 runtime_id → 外键拒绝
    let error = store
        .execute_write(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('s1', 'missing-runtime', 't', 'idle', 1, 1)",
        )
        .unwrap_err();
    assert!(is_constraint_violation(&error), "实际: {error:?}");

    // 合法链路可写
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .unwrap();
    store
        .execute_write(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('s1', 'mock', 't', 'idle', 1, 1)",
        )
        .unwrap();
    store
        .execute_write(
            "INSERT INTO messages (id, session_id, role, seq, created_at) \
             VALUES ('m1', 's1', 'user', 1, 1)",
        )
        .unwrap();

    // ON DELETE CASCADE：删除 session 级联删除 messages
    store
        .execute_write("DELETE FROM sessions WHERE id = 's1'")
        .unwrap();
    let remaining: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0, "messages.session_id 必须 ON DELETE CASCADE");

    // ON DELETE SET NULL：删除父 session 后子 session.parent_session_id 置空
    store
        .execute_write(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('parent', 'mock', 'p', 'idle', 1, 1)",
        )
        .unwrap();
    store
        .execute_write(
            "INSERT INTO sessions (id, runtime_id, parent_session_id, title, status, created_at, updated_at) \
             VALUES ('child', 'mock', 'parent', 'c', 'idle', 1, 1)",
        )
        .unwrap();
    store
        .execute_write("DELETE FROM sessions WHERE id = 'parent'")
        .unwrap();
    let parent: Option<String> = store
        .connection()
        .query_row(
            "SELECT parent_session_id FROM sessions WHERE id = 'child'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent, None, "parent_session_id 必须 ON DELETE SET NULL");
}

#[test]
fn runtimes_status_check_matches_d5_state_machine() {
    let (_dir, store) = common::open_temp_store("schema-d5");
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('r1', 'R', 'kind', '1', 1, 1)",
        )
        .unwrap();

    // 5 态全部可写（与 D5：cold → starting → ready → degraded → disabled 一一对应）
    for status in ["cold", "starting", "ready", "degraded", "disabled"] {
        store
            .execute_write(&format!(
                "UPDATE runtimes SET status = '{status}' WHERE id = 'r1'"
            ))
            .unwrap_or_else(|error| panic!("状态 {status} 应被接受: {error:?}"));
    }

    let error = store
        .execute_write("UPDATE runtimes SET status = 'bogus' WHERE id = 'r1'")
        .unwrap_err();
    assert!(
        is_constraint_violation(&error),
        "非法状态必须被 CHECK 拒绝: {error:?}"
    );
}

#[test]
fn check_enumerations_match_appendix_c() {
    let (_dir, store) = common::open_temp_store("schema-checks");
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .unwrap();

    // sessions.status：附录 C 8 态
    for status in [
        "creating",
        "idle",
        "running",
        "paused",
        "waiting_permission",
        "completed",
        "failed",
        "cancelled",
    ] {
        store
            .execute_write(&format!(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('s-{status}', 'mock', 't', '{status}', 1, 1)"
            ))
            .unwrap_or_else(|error| panic!("sessions.status={status} 应被接受: {error:?}"));
    }
    assert!(is_constraint_violation(
        &store
            .execute_write(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('bad', 'mock', 't', 'unknown', 1, 1)"
            )
            .unwrap_err()
    ));

    // runs.status：附录 C 6 态
    for status in [
        "queued",
        "running",
        "succeeded",
        "failed",
        "cancelled",
        "timeout",
    ] {
        store
            .execute_write(&format!(
                "INSERT INTO runs (id, session_id, status, started_at) \
                 VALUES ('run-{status}', 's-idle', '{status}', 1)"
            ))
            .unwrap_or_else(|error| panic!("runs.status={status} 应被接受: {error:?}"));
    }
    assert!(is_constraint_violation(
        &store
            .execute_write(
                "INSERT INTO runs (id, session_id, status, started_at) \
                 VALUES ('bad', 's-idle', 'unknown', 1)"
            )
            .unwrap_err()
    ));

    // messages.role：附录 C 4 态
    for role in ["user", "assistant", "system", "tool"] {
        store
            .execute_write(&format!(
                "INSERT INTO messages (id, session_id, role, seq, created_at) \
                 VALUES ('msg-{role}', 's-idle', '{role}', 1, 1)"
            ))
            .unwrap_or_else(|error| panic!("messages.role={role} 应被接受: {error:?}"));
    }
    assert!(is_constraint_violation(
        &store
            .execute_write(
                "INSERT INTO messages (id, session_id, role, seq, created_at) \
                 VALUES ('bad', 's-idle', 'unknown', 1, 1)"
            )
            .unwrap_err()
    ));

    // permissions：decision / scope / status
    store
        .execute_write(
            "INSERT INTO permissions (id, resource, action, decision, requested_at) \
             VALUES ('p1', 'fs.read', 'read', 'allow', 1)",
        )
        .unwrap();
    for decision in ["allow", "deny", "ask"] {
        store
            .execute_write(&format!(
                "UPDATE permissions SET decision = '{decision}' WHERE id = 'p1'"
            ))
            .unwrap_or_else(|error| panic!("permissions.decision={decision} 应被接受: {error:?}"));
    }
    for scope in ["once", "session", "always"] {
        store
            .execute_write(&format!(
                "UPDATE permissions SET scope = '{scope}' WHERE id = 'p1'"
            ))
            .unwrap_or_else(|error| panic!("permissions.scope={scope} 应被接受: {error:?}"));
    }
    for status in ["pending", "resolved", "timeout"] {
        store
            .execute_write(&format!(
                "UPDATE permissions SET status = '{status}' WHERE id = 'p1'"
            ))
            .unwrap_or_else(|error| panic!("permissions.status={status} 应被接受: {error:?}"));
    }
    assert!(is_constraint_violation(
        &store
            .execute_write("UPDATE permissions SET decision = 'maybe' WHERE id = 'p1'")
            .unwrap_err()
    ));
    assert!(is_constraint_violation(
        &store
            .execute_write("UPDATE permissions SET scope = 'forever' WHERE id = 'p1'")
            .unwrap_err()
    ));
    assert!(is_constraint_violation(
        &store
            .execute_write("UPDATE permissions SET status = 'unknown' WHERE id = 'p1'")
            .unwrap_err()
    ));

    // memories.scope
    store
        .execute_write(
            "INSERT INTO workspaces (id, name, root_path, created_at, updated_at) \
             VALUES ('w1', 'W', '/tmp', 1, 1)",
        )
        .unwrap();
    for scope in ["project", "user", "session"] {
        store
            .execute_write(&format!(
                "INSERT OR REPLACE INTO memories (id, workspace_id, scope, key, content, updated_at) \
                 VALUES ('mem-{scope}', 'w1', '{scope}', 'k', 'c', 1)"
            ))
            .unwrap_or_else(|error| panic!("memories.scope={scope} 应被接受: {error:?}"));
    }
    assert!(is_constraint_violation(
        &store
            .execute_write(
                "INSERT INTO memories (id, workspace_id, scope, key, content, updated_at) \
                 VALUES ('bad', 'w1', 'global', 'k', 'c', 1)"
            )
            .unwrap_err()
    ));

    // memories UNIQUE(workspace_id, scope, key)
    let duplicate = store
        .execute_write(
            "INSERT INTO memories (id, workspace_id, scope, key, content, updated_at) \
             VALUES ('dup', 'w1', 'project', 'k', 'c', 1)",
        )
        .unwrap_err();
    assert!(is_constraint_violation(&duplicate), "实际: {duplicate:?}");
}

#[test]
fn events_type_is_text_without_check_enumeration() {
    // D12：events.type 为 TEXT + 应用层校验（附录 B），不加 CHECK 枚举。
    let (_dir, store) = common::open_temp_store("schema-events-type");
    store
        .execute_write(
            "INSERT INTO events (id, session_id, runtime_id, seq, type, payload, ts) \
             VALUES ('e1', 's1', 'mock', 1, 'not.an.appendix.b.type', '{}', 1)",
        )
        .unwrap();
    let stored: String = store
        .connection()
        .query_row("SELECT type FROM events WHERE id = 'e1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, "not.an.appendix.b.type");
}

#[test]
fn column_defaults_match_appendix_c() {
    let (_dir, store) = common::open_temp_store("schema-defaults");
    let conn = store.connection();

    conn.execute(
        "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
         VALUES ('r1', 'R', 'k', '1', 1, 1)",
        [],
    )
    .unwrap();
    let (protocol, capabilities, config, status): (String, String, String, String) = conn
        .query_row(
            "SELECT protocol, capabilities, config, status FROM runtimes WHERE id = 'r1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(protocol, "1.0");
    assert_eq!(capabilities, "[]");
    assert_eq!(config, "{}");
    assert_eq!(status, "cold");

    conn.execute(
        "INSERT INTO workspaces (id, name, root_path, created_at, updated_at) \
         VALUES ('w1', 'W', '/tmp', 1, 1)",
        [],
    )
    .unwrap();
    let memory_files: String = conn
        .query_row(
            "SELECT memory_files FROM workspaces WHERE id = 'w1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(memory_files, "[]");

    conn.execute(
        "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
         VALUES ('s1', 'r1', 't', 'idle', 1, 1)",
        [],
    )
    .unwrap();
    let (config, token_usage): (String, String) = conn
        .query_row(
            "SELECT config, token_usage FROM sessions WHERE id = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(config, "{}");
    assert_eq!(token_usage, "{}");

    conn.execute(
        "INSERT INTO messages (id, session_id, role, seq, created_at) \
         VALUES ('m1', 's1', 'user', 1, 1)",
        [],
    )
    .unwrap();
    let content: String = conn
        .query_row("SELECT content FROM messages WHERE id = 'm1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(content, "");

    conn.execute(
        "INSERT INTO events (id, session_id, runtime_id, seq, type, payload, ts) \
         VALUES ('e1', 's1', 'r1', 1, 'log', '{}', 1)",
        [],
    )
    .unwrap();
    let v: i64 = conn
        .query_row("SELECT v FROM events WHERE id = 'e1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(v, 1, "events.v 默认 1（D4 信封）");

    conn.execute(
        "INSERT INTO permissions (id, resource, action, decision, requested_at) \
         VALUES ('p1', 'fs.read', 'read', 'ask', 1)",
        [],
    )
    .unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM permissions WHERE id = 'p1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "pending");

    conn.execute(
        "INSERT INTO backups (id, path, size_bytes, created_at) VALUES ('b1', '/tmp/b', 1, 1)",
        [],
    )
    .unwrap();
    let (encrypted, kind): (i64, String) = conn
        .query_row(
            "SELECT encrypted, kind FROM backups WHERE id = 'b1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(encrypted, 0);
    assert_eq!(kind, "manual");
}
