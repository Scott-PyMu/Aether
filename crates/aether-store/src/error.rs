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
    /// 写队列深度超过 L2 阈值：拒绝新工作准入（D8；错误码 `storage_backpressure`）。
    StorageBackpressure {
        /// 触发时的待提交条目数（队列深度）。
        depth: usize,
        /// 当前 L2 阈值。
        threshold: usize,
    },
    /// 写事务失败（批次内每个提交独立构造；`code` 为 SQLite 扩展错误码，供持久化降级判定）。
    WriteTransactionFailed {
        /// SQLite 扩展错误码（非 SQLite 错误为 `None`，如 payload 序列化失败）。
        code: Option<i32>,
        message: String,
    },
    /// 写队列已关闭（关停或写任务退出后提交被拒绝）。
    WriteQueueClosed,
    /// 空写批次（调用方错误，不产生事务）。
    EmptyWriteBatch,
    /// 写队列配置非法（容量/批量/间隔/读连接数）。
    InvalidWriteQueueConfig { reason: String },
    /// 库中事件行无法重建信封（损坏或与当前模型不兼容）。
    InvalidStoredEvent { id: String, reason: String },
    /// 内部不变量被破坏（互斥锁中毒、后台任务异常退出等）。
    Internal { reason: String },
}

impl StoreError {
    /// 写事务失败是否命中 `events.id` 主键冲突（D4：`evt.id` 幂等去重，非持久化故障）。
    ///
    /// M1-05 管线用：命中时按「重复事件丢弃 + 计数」处理，**不**走持久化降级路径。
    pub fn is_duplicate_event_id(&self) -> bool {
        matches!(
            self,
            Self::WriteTransactionFailed {
                code: Some(code),
                ..
            } if *code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        )
    }

    /// 写事务失败是否命中 `events UNIQUE(session_id, seq)` 兜底（D4：重复 seq 视为管线 bug）。
    ///
    /// M1-05 管线用：命中时计入诊断 **并按持久化失败路径处理**（D4 失败场景表）。
    pub fn is_duplicate_seq(&self) -> bool {
        matches!(
            self,
            Self::WriteTransactionFailed {
                code: Some(code),
                ..
            } if *code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        )
    }

    /// 稳定错误码（命令层 / 审计 / 诊断使用；`storage_backpressure` 为 D8 约定错误码）。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "sqlite",
            Self::Io(_) => "io",
            Self::MigrationChecksumMismatch { .. } => "migration_checksum_mismatch",
            Self::SchemaNewerThanProgram { .. } => "schema_newer_than_program",
            Self::InvalidMigrationEncoding { .. } => "invalid_migration_encoding",
            Self::InvalidMigrationSet { .. } => "invalid_migration_set",
            Self::SafeModeWriteRefused { .. } => "safe_mode_write_refused",
            Self::SafeModeUnavailable { .. } => "safe_mode_unavailable",
            Self::BackupTargetExists { .. } => "backup_target_exists",
            Self::NonUtf8Path { .. } => "non_utf8_path",
            Self::StorageBackpressure { .. } => "storage_backpressure",
            Self::WriteTransactionFailed { .. } => "write_transaction_failed",
            Self::WriteQueueClosed => "write_queue_closed",
            Self::EmptyWriteBatch => "empty_write_batch",
            Self::InvalidWriteQueueConfig { .. } => "invalid_write_queue_config",
            Self::InvalidStoredEvent { .. } => "invalid_stored_event",
            Self::Internal { .. } => "internal",
        }
    }
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
            Self::StorageBackpressure { depth, threshold } => write!(
                f,
                "存储写队列背压（storage_backpressure）：待提交 {depth} 条 > 阈值 {threshold}，拒绝新工作准入（D8 L2）"
            ),
            Self::WriteTransactionFailed { code, message } => {
                let code = match code {
                    Some(value) => format!("SQLite 扩展码 {value}"),
                    None => "非 SQLite 错误".to_owned(),
                };
                write!(f, "写事务失败（{code}）：{message}")
            }
            Self::WriteQueueClosed => {
                write!(f, "写队列已关闭：写任务已退出，提交被拒绝")
            }
            Self::EmptyWriteBatch => write!(f, "空写批次：至少需要 1 条事件"),
            Self::InvalidWriteQueueConfig { reason } => {
                write!(f, "写队列配置非法: {reason}")
            }
            Self::InvalidStoredEvent { id, reason } => {
                write!(f, "事件行无法重建信封（id={id}）: {reason}")
            }
            Self::Internal { reason } => write!(f, "存储内部错误: {reason}"),
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
            (
                StoreError::StorageBackpressure {
                    depth: 4_097,
                    threshold: 4_096,
                },
                "storage_backpressure",
            ),
            (
                StoreError::WriteTransactionFailed {
                    code: Some(7_787),
                    message: "database or disk is full".to_owned(),
                },
                "写事务失败",
            ),
            (StoreError::WriteQueueClosed, "已关闭"),
            (StoreError::EmptyWriteBatch, "空写批次"),
            (
                StoreError::InvalidWriteQueueConfig {
                    reason: "容量必须 >0".to_owned(),
                },
                "配置非法",
            ),
            (
                StoreError::InvalidStoredEvent {
                    id: "evt-1".to_owned(),
                    reason: "payload 非法 JSON".to_owned(),
                },
                "无法重建信封",
            ),
            (
                StoreError::Internal {
                    reason: "锁中毒".to_owned(),
                },
                "内部错误",
            ),
        ]
    }

    fn error_codes() -> Vec<(StoreError, &'static str)> {
        let mut expected: Vec<&'static str> = vec![
            "sqlite",
            "io",
            "migration_checksum_mismatch",
            "schema_newer_than_program",
            "invalid_migration_encoding",
            "invalid_migration_set",
            "safe_mode_write_refused",
            "safe_mode_unavailable",
            "backup_target_exists",
            "non_utf8_path",
            "storage_backpressure",
            "write_transaction_failed",
            "write_queue_closed",
            "empty_write_batch",
            "invalid_write_queue_config",
            "invalid_stored_event",
            "internal",
        ];
        let mut variants = all_variants();
        assert_eq!(
            variants.len(),
            expected.len(),
            "新增错误变体必须同步 code()"
        );
        variants
            .drain(..)
            .zip(expected.drain(..))
            .map(|((variant, _), code)| (variant, code))
            .collect()
    }

    #[test]
    fn code_is_stable_and_unique() {
        let pairs = error_codes();
        let mut codes: Vec<&str> = pairs.iter().map(|(_, code)| *code).collect();
        codes.sort_unstable();
        let count = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), count, "错误码必须唯一");
        for (variant, expected) in pairs {
            let actual = variant.code();
            assert_eq!(actual, expected, "{variant:?} 错误码不一致");
            assert!(
                actual
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "错误码必须为 snake_case: {actual}"
            );
        }
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

    #[test]
    fn write_failure_classification_distinguishes_id_and_seq() {
        let duplicate_id = StoreError::WriteTransactionFailed {
            code: Some(rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
            message: "UNIQUE constraint failed: events.id".to_owned(),
        };
        assert!(
            duplicate_id.is_duplicate_event_id(),
            "主键冲突 = evt.id 去重"
        );
        assert!(!duplicate_id.is_duplicate_seq());

        let duplicate_seq = StoreError::WriteTransactionFailed {
            code: Some(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            message: "UNIQUE constraint failed: events.session_id, events.seq".to_owned(),
        };
        assert!(duplicate_seq.is_duplicate_seq(), "UNIQUE 冲突 = seq 兜底");
        assert!(!duplicate_seq.is_duplicate_event_id());

        let disk_full = StoreError::WriteTransactionFailed {
            code: Some(rusqlite::ffi::SQLITE_FULL),
            message: "database or disk is full".to_owned(),
        };
        assert!(!disk_full.is_duplicate_event_id());
        assert!(!disk_full.is_duplicate_seq());

        let non_sqlite = StoreError::WriteTransactionFailed {
            code: None,
            message: "payload 序列化失败".to_owned(),
        };
        assert!(!non_sqlite.is_duplicate_event_id());
        assert!(!non_sqlite.is_duplicate_seq());

        assert!(!StoreError::WriteQueueClosed.is_duplicate_event_id());
        assert!(!StoreError::WriteQueueClosed.is_duplicate_seq());
    }
}
