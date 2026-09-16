//! 历史重复键审计（M1-03 DoD5；ADR-003 §6 / ADR-005 §5-4）。
//!
//! 在 0002 增量迁移落地前，只读检查既有库是否存在重复：
//!   * `events(session_id, seq)`（D4 单一 sequencer 的 DB 兜底）；
//!   * `messages(session_id, client_msg_id)`（ADR-005 幂等键；列不存在时标注 schema 状态）。
//!
//! 存在重复时 `CREATE UNIQUE INDEX` 会使迁移整体回滚（单版本单事务），必须先审计、
//! 人工处置重复数据后再迁移。
//!
//! 用法：
//!   audit_duplicates report <db>            只读统计，stdout 输出 JSON 报告
//!   audit_duplicates create-fixture <db> [--client-msg-id]
//!                                           生成带重复键的 pre-0002 演练库（仅测试用）
//!   audit_duplicates upgrade <db>           应用全部内嵌迁移（验证迁移后审计为空）
//!
//! 退出码：0 = 无重复；2 = 发现重复（报告已输出）；1 = 用法/读库错误。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_store::{migrate_with, EMBEDDED_MIGRATIONS};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        fail("用法: audit_duplicates report <db> | create-fixture <db> [--client-msg-id]");
    };
    let Some(db) = args.get(1) else {
        fail("缺少数据库路径");
    };

    match command {
        "report" => report(Path::new(db)),
        "create-fixture" => {
            let with_client_msg_id = args.iter().any(|arg| arg == "--client-msg-id");
            create_fixture(Path::new(db), with_client_msg_id);
        }
        "upgrade" => upgrade(Path::new(db)),
        other => fail(&format!("未知子命令: {other}")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("audit_duplicates: {message}");
    std::process::exit(1);
}

/// 只读打开并输出重复键报告。
fn report(path: &Path) -> ! {
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(error) => fail(&format!("只读打开失败: {error}")),
    };

    let events = duplicate_events(&conn);
    let has_client_msg_id = table_has_column(&conn, "messages", "client_msg_id");
    let messages = if has_client_msg_id {
        duplicate_client_msg_ids(&conn)
    } else {
        json!({
            "checked": false,
            "note": "messages.client_msg_id 不存在（pre-0002 schema），无历史幂等键重复",
            "duplicate_groups": 0,
            "duplicate_rows": 0,
            "top": [],
        })
    };

    let event_groups = events["duplicate_groups"].as_u64().unwrap_or(0);
    let message_groups = messages["duplicate_groups"].as_u64().unwrap_or(0);
    let report = json!({
        "database": path.display().to_string(),
        "schema": {
            "messages_client_msg_id": has_client_msg_id,
            "events_unique_session_seq": index_exists(&conn, "idx_events_session_seq_uq"),
            "messages_unique_client_msg": index_exists(&conn, "idx_messages_client_msg"),
        },
        "events": events,
        "messages": messages,
        "ok": event_groups == 0 && message_groups == 0,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    std::process::exit(if report["ok"].as_bool().unwrap_or(false) {
        0
    } else {
        2
    });
}

/// 应用全部内嵌迁移（演练与验证用；产出后置 schema 供审计对照）。
fn upgrade(path: &Path) -> ! {
    let mut conn =
        Connection::open(path).unwrap_or_else(|error| fail(&format!("打开失败: {error}")));
    let applied = migrate_with(&mut conn, EMBEDDED_MIGRATIONS)
        .unwrap_or_else(|error| fail(&format!("迁移失败: {error}")));
    let versions: Vec<i64> = applied.iter().map(|item| item.version).collect();
    println!(
        "{}",
        json!({ "database": path.display().to_string(), "applied": versions })
    );
    std::process::exit(0);
}

/// 生成 pre-0002 演练库：仅应用 0001，再注入重复键。
fn create_fixture(path: &Path, with_client_msg_id: bool) -> ! {
    if path.exists() {
        fail("演练库已存在，请先删除");
    }
    let mut conn =
        Connection::open(path).unwrap_or_else(|error| fail(&format!("创建失败: {error}")));
    migrate_with(&mut conn, &EMBEDDED_MIGRATIONS[..1])
        .unwrap_or_else(|error| fail(&format!("应用 0001 失败: {error}")));

    if with_client_msg_id {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN client_msg_id TEXT")
            .unwrap_or_else(|error| fail(&format!("补列失败: {error}")));
    }

    conn.execute(
        "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
         VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
         VALUES ('s-dup', 'mock', 't', 'idle', 1, 1)",
        [],
    )
    .unwrap();

    let mut insert_event = conn
        .prepare(
            "INSERT INTO events (id, session_id, runtime_id, seq, type, payload, ts) \
             VALUES (?1, 's-dup', 'mock', ?2, 'log', '{}', ?3)",
        )
        .unwrap();
    // seq=7 重复 2 次、seq=8 重复 3 次
    insert_event.execute(("evt-7a", 7, 1)).unwrap();
    insert_event.execute(("evt-7b", 7, 2)).unwrap();
    for index in 0..3 {
        insert_event
            .execute((format!("evt-8{index}"), 8, 3 + index))
            .unwrap();
    }
    drop(insert_event);

    if with_client_msg_id {
        let mut insert_message = conn
            .prepare(
                "INSERT INTO messages (id, session_id, client_msg_id, role, seq, created_at) \
                 VALUES (?1, 's-dup', ?2, 'user', ?3, 1)",
            )
            .unwrap();
        insert_message.execute(("msg-a", "CLIENT-DUP", 1)).unwrap();
        insert_message.execute(("msg-b", "CLIENT-DUP", 2)).unwrap();
        insert_message.execute(("msg-c", "CLIENT-OK", 3)).unwrap();
        drop(insert_message);
    }

    println!("{}", json!({ "fixture": path.display().to_string() }));
    std::process::exit(0);
}

fn duplicate_events(conn: &Connection) -> Value {
    let sql = "SELECT session_id, seq, COUNT(*) AS c FROM events \
               GROUP BY session_id, seq HAVING c > 1 ORDER BY c DESC, session_id, seq";
    let mut statement = match conn.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => fail(&format!("events 审计查询失败: {error}")),
    };
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let duplicate_groups = rows.len() as u64;
    let duplicate_rows: u64 = rows.iter().map(|(_, _, count)| (*count as u64) - 1).sum();
    json!({
        "checked": true,
        "duplicate_groups": duplicate_groups,
        "duplicate_rows": duplicate_rows,
        "top": rows.iter().map(|(session_id, seq, count)| json!({
            "session_id": session_id,
            "seq": seq,
            "count": count,
        })).collect::<Vec<_>>(),
    })
}

fn duplicate_client_msg_ids(conn: &Connection) -> Value {
    let sql = "SELECT session_id, client_msg_id, COUNT(*) AS c FROM messages \
               WHERE client_msg_id IS NOT NULL \
               GROUP BY session_id, client_msg_id HAVING c > 1 ORDER BY c DESC, session_id, client_msg_id";
    let mut statement = match conn.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => fail(&format!("messages 审计查询失败: {error}")),
    };
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let duplicate_groups = rows.len() as u64;
    let duplicate_rows: u64 = rows.iter().map(|(_, _, count)| (*count as u64) - 1).sum();
    json!({
        "checked": true,
        "duplicate_groups": duplicate_groups,
        "duplicate_rows": duplicate_rows,
        "top": rows.iter().map(|(session_id, client_msg_id, count)| json!({
            "session_id": session_id,
            "client_msg_id": client_msg_id,
            "count": count,
        })).collect::<Vec<_>>(),
    })
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    conn.query_row(&sql, [column], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        > 0
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}
