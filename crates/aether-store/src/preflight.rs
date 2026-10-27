//! 启动预检（ADR-029 v0.4 §3.5-4；M9-03 DoD-5）：**打开库 + 已应用迁移 checksum 校验**。
//!
//! 与 [`crate::Store::open`] 的分工（M9 口径）：
//! - 预检（本模块）在 `initialize` 之前执行：打开写连接（PRAGMA 全集）→ 读取
//!   `schema_migrations` → 逐项与内嵌迁移文件的 sha256 比对；
//! - **不执行** `quick_check`（DB 可打开但完整性失败属「只读安全模式」，不得致命）；
//! - **不执行**迁移（M9 不触发迁移；`0001–0009` 已是当前版本，执行仍由
//!   [`crate::Store::open`] 在预热阶段完成）。
//!
//! 失败分类由调用方映射为稳定业务码（ADR-029 §4.3）：
//! - 打开失败 / schema 高于程序 → `db_open_failed`（退出码 22）；
//! - [`super::StoreError::MigrationChecksumMismatch`] → `schema_checksum_mismatch`
//!   （退出码 22，`R11`）。

use std::path::{Path, PathBuf};

use crate::error::StoreError;
use crate::migration::{self, AppliedMigration, EMBEDDED_MIGRATIONS};
use crate::store::Store;

/// 预检结果（诊断/证据用；不参与业务判定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    /// 库文件路径。
    pub path: PathBuf,
    /// 预检前库文件是否已存在（`false` = 首次安装，将创建空库）。
    pub existed: bool,
    /// 已应用迁移（按版本升序；新库为空）。
    pub applied: Vec<AppliedMigration>,
    /// 已应用的最高版本（新库为 0）。
    pub schema_version: i64,
}

impl PreflightReport {
    /// 程序内嵌清单的最高版本（当前 `0001–0009` → 9）。
    pub fn program_version() -> i64 {
        EMBEDDED_MIGRATIONS.last().map_or(0, |file| file.version)
    }
}

/// 预检：打开库（不 `quick_check`、不迁移）+ 校验已应用迁移 checksum。
pub fn preflight(path: impl AsRef<Path>) -> Result<PreflightReport, StoreError> {
    let path = path.as_ref().to_path_buf();
    let existed = path.exists();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let conn = Store::open_writer_connection(&path)?;
    let applied = migration::applied_migrations(&conn)?;

    let program_max = PreflightReport::program_version();
    for record in &applied {
        let Some(file) = EMBEDDED_MIGRATIONS
            .iter()
            .find(|file| file.version == record.version)
        else {
            return Err(StoreError::SchemaNewerThanProgram {
                database: record.version,
                program: program_max,
            });
        };
        let recomputed = migration::checksum(file.bytes);
        if recomputed != record.checksum {
            return Err(StoreError::MigrationChecksumMismatch {
                version: record.version,
                name: file.name,
                recorded: record.checksum.clone(),
                recomputed,
            });
        }
    }

    let schema_version = applied.last().map_or(0, |record| record.version);
    drop(conn);
    Ok(PreflightReport {
        path,
        existed,
        applied,
        schema_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("aether.db");
        (dir, path)
    }

    #[test]
    fn fresh_path_passes_without_creating_schema() {
        let (_dir, path) = temp_db();
        let report = preflight(&path).expect("预检应通过");
        assert!(!report.existed, "新库文件预检前不存在");
        assert!(report.applied.is_empty(), "新库无已应用迁移");
        assert_eq!(report.schema_version, 0);
        assert!(path.exists(), "预检打开后文件存在（空库）");
        assert_eq!(
            PreflightReport::program_version(),
            EMBEDDED_MIGRATIONS.len() as i64,
            "程序版本 = 内嵌迁移数量（0001–0009）"
        );
    }

    #[test]
    fn applied_migrations_with_matching_checksums_pass() {
        let (_dir, path) = temp_db();
        {
            let store = crate::Store::open(&path).expect("打开并迁移");
            // 迁移已在 Store::open 内应用；这里确保写连接关闭后再预检。
            let _ = store.quick_check();
        }
        let report = preflight(&path).expect("checksum 一致应通过");
        assert!(report.existed);
        assert_eq!(report.applied.len(), EMBEDDED_MIGRATIONS.len());
        assert_eq!(report.schema_version, PreflightReport::program_version());
    }

    #[test]
    fn tampered_checksum_is_reported_as_mismatch() {
        let (_dir, path) = temp_db();
        {
            let _store = crate::Store::open(&path).expect("打开并迁移");
        }
        {
            let conn = rusqlite::Connection::open(&path).expect("直连");
            conn.execute(
                "UPDATE schema_migrations SET checksum = ?1 WHERE version = 1",
                params!["deadbeef"],
            )
            .expect("篡改 checksum");
        }
        match preflight(&path) {
            Err(StoreError::MigrationChecksumMismatch { version, .. }) => assert_eq!(version, 1),
            other => panic!("期望 MigrationChecksumMismatch，实际 {other:?}"),
        }
    }

    #[test]
    fn schema_newer_than_program_is_rejected() {
        let (_dir, path) = temp_db();
        {
            let _store = crate::Store::open(&path).expect("打开并迁移");
        }
        {
            let conn = rusqlite::Connection::open(&path).expect("直连");
            conn.execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
                params![999_i64, "future", 0_i64],
            )
            .expect("写入未来版本");
        }
        match preflight(&path) {
            Err(StoreError::SchemaNewerThanProgram { database, program }) => {
                assert_eq!(database, 999);
                assert_eq!(program, PreflightReport::program_version());
            }
            other => panic!("期望 SchemaNewerThanProgram，实际 {other:?}"),
        }
    }

    #[test]
    fn corrupt_header_is_an_open_error() {
        let (_dir, path) = temp_db();
        std::fs::write(&path, b"this is not a sqlite database").expect("写坏文件");
        let error = preflight(&path).expect_err("坏库应失败");
        assert!(
            !matches!(
                error,
                StoreError::MigrationChecksumMismatch { .. }
                    | StoreError::SchemaNewerThanProgram { .. }
            ),
            "坏库必须归入打开失败（db_open_failed），实际 {error:?}"
        );
    }
}
