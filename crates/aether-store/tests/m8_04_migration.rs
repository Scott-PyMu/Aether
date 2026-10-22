//! M8-04 迁移演练：P0 库（0003）→ P1 库（0008）。
//!
//! 覆盖：
//! - 0001–0003 构造 P0 库（含 sessions/messages/events/settings/workspaces 样例数据）；
//! - `VACUUM INTO` 生成 P0 备份（回退候选）；
//! - 以当前程序打开：增量应用 0004–0008（权限模式/规则、审计索引、runtimes.enabled、
//!   presets、FTS5），旧数据零丢失；
//! - FTS `rebuild` 后旧消息可检索；
//! - 幂等：再次打开不重复迁移；
//! - 备份候选 `quick_check` 通过且 schema=3（可回退 P0 程序）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_store::{migrate_with, Store, EMBEDDED_MIGRATIONS};
use rusqlite::{params, Connection};

fn seed_p0(connection: &mut Connection) {
    migrate_with(connection, &EMBEDDED_MIGRATIONS[..3]).expect("应用 0001–0003");
    connection
        .execute(
            "INSERT INTO runtimes (id, name, kind, version, protocol, capabilities, config, status, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', '1.0', '[]', '{}', 'ready', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('01J8ZQ5R0N7W9Y8X6V4T2S0K1A', 'mock', 'P0 历史会话', 'idle', 10, 10)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
             VALUES ('01J8ZQ5R0N7W9Y8X6V4T2S0K1M', '01J8ZQ5R0N7W9Y8X6V4T2S0K1A', 'user', 'P0 migration needle', 1, 11)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES ('01J8ZQ5R0N7W9Y8X6V4T2S0K1E', '01J8ZQ5R0N7W9Y8X6V4T2S0K1A', NULL, 'mock', 1, 'log', '{}', 12, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ('theme', '\"dark\"', 13)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO workspaces (id, name, root_path, created_at, updated_at) \
             VALUES ('01J8ZQ5R0N7W9Y8X6V4T2S0K1W', 'P0 工作区', 'C:/ws/p0', 14, 14)",
            [],
        )
        .unwrap();
}

#[test]
fn p0_database_upgrades_to_p1_without_data_loss_and_backup_is_rollback_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("aether.db");
    let backup = dir.path().join("p0-backup.db");

    {
        let mut connection = Connection::open(&db).unwrap();
        seed_p0(&mut connection);
        connection
            .execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
            .expect("P0 备份");
    }

    // 当前程序打开 → 增量迁移到 0008。
    let store = Store::open(&db).expect("P1 迁移");
    let applied = store.applied_migrations().unwrap();
    assert_eq!(
        applied.iter().map(|row| row.version).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        "P0 → P1 必须增量应用 0004–0009"
    );
    let connection = store.connection();

    // 旧数据保留。
    let title: String = connection
        .query_row(
            "SELECT title FROM sessions WHERE id = '01J8ZQ5R0N7W9Y8X6V4T2S0K1A'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "P0 历史会话");
    let messages: i64 = connection
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 1);
    let events: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 1);

    // P1 新增列/表落地。
    let enabled: i64 = connection
        .query_row("SELECT enabled FROM runtimes WHERE id = 'mock'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(enabled, 1, "0006 runtimes.enabled 默认 1");
    let preset_id: Option<String> = connection
        .query_row(
            "SELECT preset_id FROM sessions WHERE id = '01J8ZQ5R0N7W9Y8X6V4T2S0K1A'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(preset_id.is_none(), "0007 sessions.preset_id 历史行 NULL");
    let permission_mode: String = connection
        .query_row(
            "SELECT permission_mode FROM sessions WHERE id = '01J8ZQ5R0N7W9Y8X6V4T2S0K1A'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(permission_mode, "workspace_write", "0004 默认档位");
    let presets: i64 = connection
        .query_row("SELECT COUNT(*) FROM presets", [], |row| row.get(0))
        .unwrap();
    assert_eq!(presets, 3, "0007 三条内置预设播种");
    let fts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('sessions_fts','messages_fts')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fts, 2, "0008 FTS5 虚拟表");

    // FTS rebuild 后旧消息可检索（迁移内已 rebuild）。
    let hit: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '\"migration\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hit, 1, "旧消息必须进入 FTS 索引");
    let quick: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(quick, "ok");

    // 幂等：再次打开不重复迁移（记录仍为 9 条，无新增）。
    drop(store);
    let reopened = Store::open(&db).expect("再次打开");
    assert_eq!(reopened.applied_migrations().unwrap().len(), 9);

    // P0 备份可作为回退候选：schema=3、数据完整、quick_check ok。
    let backup_conn = Connection::open(&backup).unwrap();
    let max_version: i64 = backup_conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(max_version, 3, "备份保持 P0 schema 0003");
    let backup_messages: i64 = backup_conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(backup_messages, 1);
    let backup_quick: String = backup_conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(backup_quick, "ok");
}
