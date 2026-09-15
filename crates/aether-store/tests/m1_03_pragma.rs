//! M1-03 DoD1：PRAGMA 全集断言。
//!
//! `journal_mode=wal`、`synchronous=normal`、`foreign_keys=on`、`busy_timeout=5000`、
//! `wal_autocheckpoint=1000`、`journal_size_limit=67108864`、`cache_size=-32000`
//! （另按设计 D3 断言 `temp_store=MEMORY`）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_store::pragma;
use aether_store::Store;
use rusqlite::{Connection, OpenFlags};

#[test]
fn pragmas_match_d3_contract_after_open() {
    let (_dir, store) = common::open_temp_store("pragma-contract");
    let snapshot = store.pragma_snapshot().unwrap();

    assert_eq!(snapshot.journal_mode, "wal", "journal_mode");
    assert_eq!(snapshot.synchronous, 1, "synchronous=NORMAL");
    assert_eq!(snapshot.foreign_keys, 1, "foreign_keys=ON");
    assert_eq!(snapshot.busy_timeout, 5000, "busy_timeout");
    assert_eq!(snapshot.wal_autocheckpoint, 1000, "wal_autocheckpoint");
    assert_eq!(
        snapshot.journal_size_limit, 67_108_864,
        "journal_size_limit"
    );
    assert_eq!(snapshot.cache_size, -32_000, "cache_size");
    assert_eq!(snapshot.temp_store, 2, "temp_store=MEMORY（设计 D3）");
    assert!(snapshot.matches_contract(), "快照必须整体满足 D3 契约");
}

#[test]
fn pragmas_are_applied_on_every_new_connection() {
    let dir = common::temp_dir("pragma-reopen");
    let path = common::db_path(&dir);

    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    assert!(first.pragma_snapshot().unwrap().matches_contract());
    assert!(second.pragma_snapshot().unwrap().matches_contract());
}

#[test]
fn journal_mode_persists_in_file_wal_header() {
    let dir = common::temp_dir("pragma-persist");
    let path = common::db_path(&dir);
    {
        let _store = Store::open(&path).unwrap();
    }
    // 重新以裸连接打开：不执行任何 PRAGMA，journal_mode 应从文件头读出 wal。
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn read_only_connection_keeps_contract_and_is_restricted_to_reads() {
    let dir = common::temp_dir("pragma-read-only");
    let path = common::db_path(&dir);
    {
        let store = Store::open(&path).unwrap();
        store
            .execute_write("INSERT INTO settings (key, value, updated_at) VALUES ('k', '1', 1)")
            .unwrap();
    }

    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    pragma::apply_read_only(&conn).unwrap();

    let snapshot = pragma::snapshot(&conn).unwrap();
    assert_eq!(snapshot.journal_mode, "wal", "只读连接不得把 WAL 降级");
    assert_eq!(snapshot.synchronous, 1);
    assert_eq!(snapshot.foreign_keys, 1);
    assert_eq!(snapshot.busy_timeout, 5000);
    assert_eq!(snapshot.wal_autocheckpoint, 1000);
    assert_eq!(snapshot.journal_size_limit, 67_108_864);
    assert_eq!(snapshot.cache_size, -32_000);
    assert_eq!(snapshot.temp_store, 2);

    let write = conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES ('x', '1', 1)",
        [],
    );
    assert!(write.is_err(), "只读连接必须拒绝写入");
}
