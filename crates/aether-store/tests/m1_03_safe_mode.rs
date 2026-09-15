//! M1-03 DoD4：损坏库样本 → 安全模式（只读 + 备份/导出入口），拒绝写入。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use aether_store::{Store, StoreError, StoreMode};
use rusqlite::Connection;

/// 构造损坏库样本：两张表写满多页 → TRUNCATE checkpoint → 破坏最后一页页头。
fn build_corrupt_store(path: &Path) {
    {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE stable_data (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
             CREATE TABLE fragile_data (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
        )
        .unwrap();
        for value in ["keep-1", "keep-2"] {
            conn.execute("INSERT INTO stable_data (value) VALUES (?1)", [value])
                .unwrap();
        }
        for index in 0..800 {
            conn.execute(
                "INSERT INTO fragile_data (payload) VALUES (?1)",
                [format!("row-{index:04}-{}", "x".repeat(180))],
            )
            .unwrap();
        }
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    common::corrupt_last_page(path);
}

#[test]
fn corrupt_database_enters_safe_mode_and_refuses_writes() {
    let dir = common::temp_dir("safe-mode");
    let path = common::db_path(&dir);
    build_corrupt_store(&path);
    let hash_before = common::sha256_file(&path);

    let store = Store::open(&path).unwrap();
    match store.mode() {
        StoreMode::SafeMode { reason } => {
            assert!(
                reason.contains("quick_check"),
                "安全模式原因应指向启动 quick_check: {reason}"
            );
        }
        other => panic!("损坏库必须进入安全模式，实际: {other:?}"),
    }
    assert!(store.is_safe_mode());
    assert!(!store.quick_check().ok, "损坏库 quick_check 必须失败");

    // 拒绝写入：门禁错误类型 + 底层只读连接双重防护
    let error = store
        .execute_write("INSERT INTO stable_data (value) VALUES ('nope')")
        .unwrap_err();
    assert!(
        matches!(error, StoreError::SafeModeWriteRefused { .. }),
        "实际: {error:?}"
    );
    let raw = store
        .connection()
        .execute("INSERT INTO stable_data (value) VALUES ('nope')", []);
    assert!(raw.is_err(), "绕过门禁直连只读连接也必须被 SQLite 拒绝");

    // 备份/导出入口：可读数据导出必须可用且跳过损坏表
    let export_path = dir.path().join("export.jsonl");
    let report = store.export_readable(&export_path).unwrap();
    let stable = report
        .tables
        .iter()
        .find(|table| table.table == "stable_data")
        .unwrap_or_else(|| panic!("导出报告缺少 stable_data: {report:?}"));
    assert!(stable.error.is_none(), "stable_data 应可读: {stable:?}");
    assert!(stable.rows >= 2);
    assert!(
        report
            .tables
            .iter()
            .any(|table| table.table == "fragile_data"),
        "受损表也必须出现在导出报告中: {report:?}"
    );
    assert!(report.rows_written >= 2);
    let exported = fs::read_to_string(&export_path).unwrap();
    assert!(exported.contains("keep-1"), "导出内容应包含可读数据");

    // 备份入口可用（损坏库上 VACUUM INTO 可能失败；入口必须返回 Result 而不 panic）
    let backup_path = dir.path().join("backup.db");
    match store.backup_to(&backup_path) {
        Ok(size) => {
            println!("损坏库 VACUUM INTO 成功（部分页可读）: {size} 字节");
            assert!(size > 0);
        }
        Err(error) => {
            println!("损坏库 VACUUM INTO 返回错误（入口仍可用）: {error}");
            let message = error.to_string();
            assert!(
                message.contains("SQLite")
                    || message.contains("损坏")
                    || message.contains("quick_check"),
                "备份失败必须是可诊断的存储错误: {error}"
            );
        }
    }

    // 只读打开不得修改主库文件内容
    assert_eq!(
        common::sha256_file(&path),
        hash_before,
        "安全模式必须保持主库文件不变"
    );
}

#[test]
fn safe_mode_reason_is_stable_across_reopen() {
    let dir = common::temp_dir("safe-mode-reopen");
    let path = common::db_path(&dir);
    build_corrupt_store(&path);

    let first = Store::open(&path).unwrap();
    let first_reason = first.safe_mode_reason().unwrap().to_owned();
    drop(first);

    let second = Store::open(&path).unwrap();
    assert!(second.is_safe_mode());
    assert_eq!(second.safe_mode_reason().unwrap(), first_reason);
    assert!(second.safe_mode_reason().unwrap().contains("quick_check"));
}

#[test]
fn garbage_file_is_not_writable_even_if_safe_mode_opens() {
    let dir = common::temp_dir("safe-mode-garbage");
    let path = dir.path().join("garbage.db");
    fs::write(&path, vec![0u8; 4096]).unwrap();

    match Store::open(&path) {
        Ok(store) => {
            assert!(store.is_safe_mode(), "非数据库文件应降级为安全模式");
            assert!(!store.quick_check().ok);
            assert!(store.execute_write("CREATE TABLE t (id TEXT)").is_err());
        }
        Err(error) => {
            assert!(
                matches!(
                    error,
                    StoreError::SafeModeUnavailable { .. } | StoreError::Sqlite(_)
                ),
                "实际: {error:?}"
            );
        }
    }
}

#[test]
fn empty_file_is_created_as_fresh_database() {
    let dir = common::temp_dir("safe-mode-empty");
    let path = dir.path().join("fresh.db");
    fs::write(&path, []).unwrap();

    let store = Store::open(&path).unwrap();
    assert!(matches!(store.mode(), StoreMode::ReadWrite));
    assert!(store.quick_check().ok);
    assert_eq!(store.applied_migrations().unwrap().len(), 1);
}
