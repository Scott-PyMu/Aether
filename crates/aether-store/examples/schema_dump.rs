//! 附录 C 结构核对转储（M1-03 DoD3）。
//!
//! 在内存库执行全部内嵌迁移（0001 → 0002 → …）后，把真实 schema 结构以纯 JSON
//! 输出到 stdout，由 `scripts/test/m1-03/verify-m1-03.mjs` 与设计文档附录 C 逐项比对：
//! 表 / 列（类型、NOT NULL、默认值、主键）/ 外键（含 ON DELETE）/ 索引（含 UNIQUE）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use rusqlite::Connection;
use serde_json::{json, Value};

fn main() {
    let mut conn = Connection::open_in_memory().unwrap();
    aether_store::migrate(&mut conn).unwrap();

    let mut tables = Vec::new();
    for name in table_names(&conn) {
        tables.push(json!({
            "name": name,
            "columns": columns(&conn, &name),
            "foreign_keys": foreign_keys(&conn, &name),
            "indexes": indexes(&conn, &name),
        }));
    }
    println!("{}", json!({ "tables": tables }));
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

fn columns(conn: &Connection, table: &str) -> Vec<Value> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({})", quote(table)))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "notnull": row.get::<_, i64>(3)?,
                "dflt_value": row.get::<_, Option<String>>(4)?,
                "pk": row.get::<_, i64>(5)?,
            }))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn foreign_keys(conn: &Connection, table: &str) -> Vec<Value> {
    let mut statement = conn
        .prepare(&format!("PRAGMA foreign_key_list({})", quote(table)))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "from": row.get::<_, String>(3)?,
                "table": row.get::<_, String>(2)?,
                "to": row.get::<_, Option<String>>(4)?,
                "on_delete": row.get::<_, String>(6)?,
            }))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn indexes(conn: &Connection, table: &str) -> Vec<Value> {
    let mut statement = conn
        .prepare(&format!("PRAGMA index_list({})", quote(table)))
        .unwrap();
    let listed: Vec<(String, i64, String)> = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(statement);

    let mut result = Vec::new();
    for (name, unique, origin) in listed {
        result.push(json!({
            "name": name,
            "unique": unique,
            "origin": origin,
            "columns": index_columns(conn, &name),
        }));
    }
    result
}

fn index_columns(conn: &Connection, index: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA index_info({})", quote(index)))
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(2))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
