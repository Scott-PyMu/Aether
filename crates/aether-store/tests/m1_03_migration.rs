//! M1-03 DoD2：迁移 0 → 最新幂等；`schema_migrations` 记录 version+checksum；
//! 篡改任一迁移文件后启动被拒绝。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::PathBuf;

use aether_store::{
    checksum, migrate, migrate_with, MigrationFile, Store, StoreError, StoreMode,
    EMBEDDED_MIGRATIONS,
};
use rusqlite::Connection;

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../migrations")
}

fn migration_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn migration_zero_to_latest_is_idempotent() {
    let dir = common::temp_dir("migration-idempotent");
    let path = common::db_path(&dir);

    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.mode(), &StoreMode::ReadWrite);
        let applied = store.applied_migrations().unwrap();
        assert_eq!(
            applied.len(),
            EMBEDDED_MIGRATIONS.len(),
            "0 → 最新应恰好应用全部内嵌迁移"
        );
        assert_eq!(
            applied.iter().map(|item| item.version).collect::<Vec<_>>(),
            vec![1, 2],
            "迁移必须按 0001 → 0002 顺序应用"
        );
        assert!(applied.iter().all(|item| item.applied_at > 0));
    }

    {
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.applied_migrations().unwrap().len(),
            EMBEDDED_MIGRATIONS.len(),
            "重开不得重复记录"
        );
    }

    // 直接调用 migrate：第二次必须为空（幂等）
    let mut conn = Connection::open(&path).unwrap();
    let first = migrate(&mut conn).unwrap();
    assert!(first.is_empty(), "已是最新时不应再有待应用迁移");
    let second = migrate(&mut conn).unwrap();
    assert!(second.is_empty());
    assert_eq!(migration_count(&conn), EMBEDDED_MIGRATIONS.len() as i64);
}

#[test]
fn existing_0001_only_database_is_upgraded_by_0002_increment() {
    // M1-03 DoD5（ADR-004 决策 6）：既有库（已应用 0001）经 0002+ 增量迁移补齐。
    let dir = common::temp_dir("migration-existing-0001");
    let path = common::db_path(&dir);

    // 构造「旧库」：仅应用 0001（0002 尚未发布时的状态）。
    {
        let mut conn = Connection::open(&path).unwrap();
        let applied = migrate_with(&mut conn, &EMBEDDED_MIGRATIONS[..1]).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].version, 1);
    }

    // 以当前程序打开：应增量应用 0002，且保留 0001 的记录（不得重跑/修改）。
    let store = Store::open(&path).unwrap();
    let applied = store.applied_migrations().unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].version, 1);
    assert_eq!(applied[1].version, 2);

    let conn = store.connection();
    let has_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'client_msg_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_column, 1, "0002 必须补齐 messages.client_msg_id 列");

    let unique_indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('events') WHERE name = 'idx_events_session_seq_uq' AND \"unique\" = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        unique_indexes, 1,
        "0002 必须建立 events UNIQUE(session_id, seq)"
    );

    let redundant: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_events_session_seq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(redundant, 0, "冗余的 idx_events_session_seq 必须删除");
}

#[test]
fn recorded_checksum_equals_file_sha256() {
    let (_dir, store) = common::open_temp_store("migration-checksum");
    let applied = store.applied_migrations().unwrap();
    assert_eq!(applied.len(), EMBEDDED_MIGRATIONS.len());

    for record in &applied {
        let file = EMBEDDED_MIGRATIONS
            .iter()
            .find(|item| item.version == record.version)
            .unwrap();
        let on_disk = fs::read(migrations_dir().join(file.name)).unwrap();
        assert_eq!(
            record.checksum,
            checksum(&on_disk),
            "库中 checksum 必须是迁移文件字节的 sha256"
        );
        assert_eq!(
            record.checksum,
            checksum(file.bytes),
            "内嵌字节必须与磁盘文件一致"
        );
        assert_eq!(record.checksum.len(), 64);
        assert!(record.checksum.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[test]
fn embedded_manifest_covers_every_migration_file() {
    let mut files: Vec<(i64, String)> = fs::read_dir(migrations_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sql"))
        .map(|name| {
            let version: i64 = name[..4].parse().unwrap();
            (version, name)
        })
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        EMBEDDED_MIGRATIONS.len(),
        "所有迁移文件都必须内嵌"
    );
    for ((version, name), embedded) in files.iter().zip(EMBEDDED_MIGRATIONS) {
        assert_eq!(*version, embedded.version, "{name} 版本与文件名前缀不一致");
        assert_eq!(name, embedded.name);
        let bytes = fs::read(migrations_dir().join(name)).unwrap();
        assert_eq!(bytes, embedded.bytes, "{name} 内嵌内容与磁盘不一致");
    }
}

#[test]
fn tampered_migration_file_is_refused_on_start() {
    // 场景：库已按原文件迁移过；程序升级时内嵌的迁移文件被篡改 → 启动必须拒绝。
    let dir = common::temp_dir("migration-tamper");
    let path = common::db_path(&dir);
    let original_checksum = {
        let store = Store::open(&path).unwrap();
        store.applied_migrations().unwrap()[0].checksum.clone()
    };

    let tampered: &[MigrationFile] = &[MigrationFile {
        version: 1,
        name: "0001_init.sql",
        bytes: b"-- tampered\nCREATE TABLE tampered_marker (id TEXT);\n",
    }];

    let mut conn = Connection::open(&path).unwrap();
    let error = migrate_with(&mut conn, tampered).unwrap_err();
    match error {
        StoreError::MigrationChecksumMismatch {
            version,
            name,
            recorded,
            recomputed,
        } => {
            assert_eq!(version, 1);
            assert_eq!(name, "0001_init.sql");
            assert_eq!(recorded, original_checksum);
            assert_eq!(recomputed, checksum(tampered[0].bytes));
            assert_ne!(recorded, recomputed);
        }
        other => panic!("应为 MigrationChecksumMismatch，实际: {other:?}"),
    }

    // 拒绝后：不得执行任何迁移语句、不得改动版本记录
    let marker: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'tampered_marker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(marker, 0, "被拒绝的迁移不得执行");
    assert_eq!(
        migration_count(&conn),
        EMBEDDED_MIGRATIONS.len() as i64,
        "被拒绝的迁移不得写版本记录"
    );
}

#[test]
fn tampered_record_in_database_is_refused_by_open() {
    // 场景：库中记录的 checksum 被改写（等价于任一迁移文件被篡改的检测路径）→ 启动拒绝。
    let dir = common::temp_dir("migration-record-tamper");
    let path = common::db_path(&dir);
    {
        let store = Store::open(&path).unwrap();
        store
            .connection()
            .execute(
                "UPDATE schema_migrations SET checksum = 'deadbeef' WHERE version = 1",
                [],
            )
            .unwrap();
    }

    match Store::open(&path) {
        Err(StoreError::MigrationChecksumMismatch { version, .. }) => assert_eq!(version, 1),
        Err(other) => panic!("应拒绝启动（MigrationChecksumMismatch），实际: {other:?}"),
        Ok(_) => panic!("篡改迁移记录后启动必须被拒绝"),
    }
}

#[test]
fn database_newer_than_program_is_refused() {
    let dir = common::temp_dir("migration-newer");
    let path = common::db_path(&dir);
    {
        let store = Store::open(&path).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (99, 'x', 0)",
                [],
            )
            .unwrap();
    }

    match Store::open(&path) {
        Err(StoreError::SchemaNewerThanProgram {
            database: 99,
            program: 2,
        }) => {}
        Err(other) => panic!("应为 SchemaNewerThanProgram，实际: {other:?}"),
        Ok(_) => panic!("库版本高于程序时必须拒绝启动"),
    }
}

#[test]
fn invalid_migration_sets_are_rejected() {
    let mut conn = Connection::open_in_memory().unwrap();
    let empty: &[MigrationFile] = &[];
    assert!(matches!(
        migrate_with(&mut conn, empty),
        Err(StoreError::InvalidMigrationSet { .. })
    ));

    let unordered: &[MigrationFile] = &[
        MigrationFile {
            version: 2,
            name: "0002_x.sql",
            bytes: b"SELECT 1;",
        },
        MigrationFile {
            version: 1,
            name: "0001_x.sql",
            bytes: b"SELECT 1;",
        },
    ];
    assert!(matches!(
        migrate_with(&mut conn, unordered),
        Err(StoreError::InvalidMigrationSet { .. })
    ));

    let bad_encoding: &[MigrationFile] = &[MigrationFile {
        version: 1,
        name: "0001_x.sql",
        bytes: &[0xFF, 0xFE, 0xFD],
    }];
    assert!(matches!(
        migrate_with(&mut conn, bad_encoding),
        Err(StoreError::InvalidMigrationEncoding { .. })
    ));
}
