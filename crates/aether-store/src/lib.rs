//! Aether 存储层（设计 D3：单文件 SQLite + WAL + 单写任务）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! M1-03 交付：rusqlite 打开 + PRAGMA 全集（[`pragma`]）、迁移框架
//! （[`migration`]：schema_migrations + 文件 sha256，禁用 `PRAGMA user_version`）、
//! 附录 C DDL（`migrations/0001_init.sql`）、启动 `quick_check` 与安全模式
//! （只读 + 备份/导出入口，[`store`]）。
//!
//! 硬约束（AGENTS.md §2.2）：禁止 `unwrap()` / `expect()` / `panic!()`
//! （经 workspace clippy lint 强制；测试代码在 crate 级显式豁免）。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod migration;
pub mod pragma;
pub mod store;

pub use error::StoreError;
pub use migration::{
    checksum, migrate, migrate_with, AppliedMigration, MigrationFile, EMBEDDED_MIGRATIONS,
    MIGRATIONS_TABLE,
};
pub use pragma::PragmaSnapshot;
pub use store::{quick_check, ExportReport, IntegrityReport, Store, StoreMode, TableExport};

#[cfg(test)]
mod tests {
    #[test]
    fn store_depends_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
    }
}
