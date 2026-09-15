//! PRAGMA 全集（设计 D3）：在连接打开时执行，**不在迁移文件中**。
//!
//! 常量与断言口径见 M1-03 DoD1：
//! `journal_mode=wal`、`synchronous=normal`、`foreign_keys=on`、`busy_timeout=5000`、
//! `wal_autocheckpoint=1000`、`journal_size_limit=67108864`、`cache_size=-32000`
//! （另按设计 D3 一并执行 `temp_store=MEMORY`）。

use std::time::Duration;

use rusqlite::Connection;

use crate::error::StoreError;

/// `journal_mode=WAL`。
pub const JOURNAL_MODE: &str = "wal";
/// `synchronous=NORMAL`。
pub const SYNCHRONOUS_NORMAL: i64 = 1;
/// `foreign_keys=ON`。
pub const FOREIGN_KEYS_ON: i64 = 1;
/// `busy_timeout=5000`（毫秒）。
pub const BUSY_TIMEOUT_MS: i64 = 5_000;
/// `wal_autocheckpoint=1000`（页）。
pub const WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;
/// `journal_size_limit=67108864`（字节，64MiB）。
pub const JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864;
/// `temp_store=MEMORY`（SQLite 枚举值 2）。
pub const TEMP_STORE_MEMORY: i64 = 2;
/// `cache_size=-32000`（KiB；负值表示 KiB 单位）。
pub const CACHE_SIZE_KIB: i64 = -32_000;

/// 对读写连接执行 PRAGMA 全集（含 `journal_mode=WAL`）。
pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "journal_mode", JOURNAL_MODE)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;
    conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    Ok(())
}

/// 对只读连接（安全模式）执行连接级 PRAGMA。
///
/// `journal_mode` 的变更需要写权限，只读连接不设置（其值由 [`PragmaSnapshot`] 断言应为 `wal`）；
/// 其余 PRAGMA 均为连接级设置，不修改库文件。
pub fn apply_read_only(conn: &Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;
    conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    Ok(())
}

/// PRAGMA 现值快照（DoD1 断言与诊断共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaSnapshot {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: i64,
    pub busy_timeout: i64,
    pub wal_autocheckpoint: i64,
    pub journal_size_limit: i64,
    pub temp_store: i64,
    pub cache_size: i64,
}

impl PragmaSnapshot {
    /// 是否为 D3 / DoD1 约定的 PRAGMA 全集取值。
    pub fn matches_contract(&self) -> bool {
        self.journal_mode.eq_ignore_ascii_case(JOURNAL_MODE)
            && self.synchronous == SYNCHRONOUS_NORMAL
            && self.foreign_keys == FOREIGN_KEYS_ON
            && self.busy_timeout == BUSY_TIMEOUT_MS
            && self.wal_autocheckpoint == WAL_AUTOCHECKPOINT_PAGES
            && self.journal_size_limit == JOURNAL_SIZE_LIMIT_BYTES
            && self.temp_store == TEMP_STORE_MEMORY
            && self.cache_size == CACHE_SIZE_KIB
    }
}

/// 读取当前连接的 PRAGMA 现值。
pub fn snapshot(conn: &Connection) -> Result<PragmaSnapshot, StoreError> {
    Ok(PragmaSnapshot {
        journal_mode: conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?,
        synchronous: conn.pragma_query_value(None, "synchronous", |row| row.get(0))?,
        foreign_keys: conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
        busy_timeout: conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?,
        wal_autocheckpoint: conn
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))?,
        journal_size_limit: conn
            .pragma_query_value(None, "journal_size_limit", |row| row.get(0))?,
        temp_store: conn.pragma_query_value(None, "temp_store", |row| row.get(0))?,
        cache_size: conn.pragma_query_value(None, "cache_size", |row| row.get(0))?,
    })
}
