//! 存储层错误（AGENTS.md §2.2：不使用 `unwrap()` / `expect()` / `panic!()`，全部路径返回 `Result`）。

use std::fmt;
use std::path::PathBuf;

/// 存储层错误。
#[derive(Debug)]
pub enum StoreError {
    /// SQLite 层错误（打开、执行、完整性检查等）。
    Sqlite(rusqlite::Error),
    /// 文件系统错误（目录创建、导出落盘、备份产物读取）。
    Io(std::io::Error),
    /// 已应用迁移与当前程序内嵌文件的 sha256 不一致（D3：拒绝启动，防历史被改）。
    MigrationChecksumMismatch {
        version: i64,
        name: &'static str,
        /// 数据库中记录的 checksum。
        recorded: String,
        /// 当前内嵌迁移文件重新计算的 sha256。
        recomputed: String,
    },
    /// 数据库记录的迁移版本高于程序内嵌清单（拒绝启动，避免降级破坏数据）。
    SchemaNewerThanProgram {
        /// 数据库中出现的版本。
        database: i64,
        /// 当前程序内嵌清单的最大版本。
        program: i64,
    },
    /// 迁移文件不是合法 UTF-8，无法作为 SQL 执行。
    InvalidMigrationEncoding { name: &'static str },
    /// 迁移清单非法（空、版本重复或非升序）。
    InvalidMigrationSet { reason: String },
    /// 安全模式（只读）下拒绝写入。
    SafeModeWriteRefused { reason: String },
    /// 库损坏且只读连接也无法建立（安全模式不可用；只能从备份恢复）。
    SafeModeUnavailable { path: PathBuf, reason: String },
    /// 备份目标已存在（`VACUUM INTO` 不允许覆盖已有文件）。
    BackupTargetExists { path: PathBuf },
    /// 路径无法转换为 UTF-8（SQLite 文件名接口要求）。
    NonUtf8Path { path: PathBuf },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "SQLite 错误: {error}"),
            Self::Io(error) => write!(f, "文件系统错误: {error}"),
            Self::MigrationChecksumMismatch {
                version,
                name,
                recorded,
                recomputed,
            } => write!(
                f,
                "迁移文件校验失败，拒绝启动：{name}（v{version}）记录 sha256={recorded}，\
                 当前文件 sha256={recomputed}（D3：已应用迁移的历史不得被修改）"
            ),
            Self::SchemaNewerThanProgram { database, program } => write!(
                f,
                "数据库 schema 版本 v{database} 高于当前程序支持的 v{program}，拒绝启动（请升级程序）"
            ),
            Self::InvalidMigrationEncoding { name } => {
                write!(f, "迁移文件不是合法 UTF-8：{name}")
            }
            Self::InvalidMigrationSet { reason } => write!(f, "迁移清单非法: {reason}"),
            Self::SafeModeWriteRefused { reason } => write!(
                f,
                "安全模式（只读）拒绝写入：{reason}（请先备份/导出并从备份恢复）"
            ),
            Self::SafeModeUnavailable { path, reason } => write!(
                f,
                "数据库损坏且无法进入安全模式（{}）: {reason}；请从备份恢复",
                path.display()
            ),
            Self::BackupTargetExists { path } => {
                write!(f, "备份目标已存在，拒绝覆盖：{}", path.display())
            }
            Self::NonUtf8Path { path } => {
                write!(f, "路径不是合法 UTF-8，无法传给 SQLite：{}", path.display())
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::path::PathBuf;

    use super::StoreError;

    fn all_variants() -> Vec<(StoreError, &'static str)> {
        vec![
            (
                StoreError::from(rusqlite::Error::InvalidQuery),
                "SQLite 错误",
            ),
            (
                StoreError::from(io::Error::new(io::ErrorKind::NotFound, "missing")),
                "文件系统错误",
            ),
            (
                StoreError::MigrationChecksumMismatch {
                    version: 1,
                    name: "0001_init.sql",
                    recorded: "aa".to_owned(),
                    recomputed: "bb".to_owned(),
                },
                "拒绝启动",
            ),
            (
                StoreError::SchemaNewerThanProgram {
                    database: 2,
                    program: 1,
                },
                "高于当前程序",
            ),
            (
                StoreError::InvalidMigrationEncoding { name: "0002_x.sql" },
                "UTF-8",
            ),
            (
                StoreError::InvalidMigrationSet {
                    reason: "空".to_owned(),
                },
                "迁移清单非法",
            ),
            (
                StoreError::SafeModeWriteRefused {
                    reason: "quick_check 失败".to_owned(),
                },
                "拒绝写入",
            ),
            (
                StoreError::SafeModeUnavailable {
                    path: PathBuf::from("aether.db"),
                    reason: "file is not a database".to_owned(),
                },
                "安全模式",
            ),
            (
                StoreError::BackupTargetExists {
                    path: PathBuf::from("backup.db"),
                },
                "拒绝覆盖",
            ),
            (
                StoreError::NonUtf8Path {
                    path: PathBuf::from("bad.db"),
                },
                "UTF-8",
            ),
        ]
    }

    #[test]
    fn display_covers_every_variant() {
        for (error, expected) in all_variants() {
            let rendered = error.to_string();
            assert!(
                rendered.contains(expected),
                "「{rendered}」应包含「{expected}」"
            );
        }
    }

    #[test]
    fn source_exposes_wrapped_errors_only() {
        let sqlite = StoreError::from(rusqlite::Error::InvalidQuery);
        assert!(sqlite.source().is_some());
        let io = StoreError::from(io::Error::other("boom"));
        assert!(io.source().is_some());
        let logic = StoreError::InvalidMigrationSet {
            reason: "x".to_owned(),
        };
        assert!(logic.source().is_none());
    }
}
