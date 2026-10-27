//! Aether 存储层（设计 D3：单文件 SQLite + WAL + 单写任务）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! M1-03 交付：rusqlite 打开 + PRAGMA 全集（[`pragma`]）、迁移框架
//! （[`migration`]：schema_migrations + 文件 sha256，禁用 `PRAGMA user_version`）、
//! 附录 C DDL（`migrations/0001_init.sql`）、启动 `quick_check` 与安全模式
//! （只读 + 备份/导出入口，[`store`]）。
//! M1-04 交付：单写队列与 group commit（[`write_queue`]：`mpsc(4096)`、
//! 16ms/256 条批量提交、4 读连接池、L1/L2 背压信号与 `storage_backpressure` 准入接口）。
//! M2-06 交付：关闭序列五步（drain → 关读连接 → `wal_checkpoint(TRUNCATE)` →
//! 关写连接 → 退出，`write_queue` 的 shadow 日志）+ WAL checkpoint 退避纪律
//! （[`checkpoint`]：读锁失败退避重试 + 诊断；运行期 256MB 强制入口）。
//!
//! 硬约束（AGENTS.md §2.2）：禁止 `unwrap()` / `expect()` / `panic!()`
//! （经 workspace clippy lint 强制；测试代码在 crate 级显式豁免）。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod archive;
pub mod backup;
pub mod checkpoint;
pub mod error;
pub mod migration;
pub mod ops;
pub mod pragma;
pub mod preflight;
pub mod store;
pub mod write_queue;

pub use archive::{
    archive_line_whitelist, archive_retention_batch, audit_archive_file_name,
    cleanup_archive_temps, compact_date_utc, export_audit_archive, export_full_archive,
    export_session_archive, full_archive_file_name, retention_archive_file_name,
    select_retention_batch, session_archive_file_name, unique_archive_path, verify_archive_file,
    ArchiveCounts, ArchiveManifest, ArchiveReport, ArchiveSanitizer, ArchiveVerification,
    RetentionEventRow, ARCHIVE_BUFFER_BYTES, ARCHIVE_FORMAT_VERSION, ARCHIVE_MANIFEST_KIND,
    ARCHIVE_PARTIAL_INFIX, ARCHIVE_SOURCE_ARCHIVE_EXPORT, ARCHIVE_SOURCE_AUDIT_EXPORT,
    ARCHIVE_SOURCE_AUTO_RETENTION, ARCHIVE_SOURCE_SESSION_EXPORT, ARCHIVE_TEMP_INFIX,
    RETENTION_BATCH_MAX, RETENTION_EVENT_TYPES,
};

pub use backup::{
    backup_file_name, backup_file_name_for, backup_file_name_for_with_extension,
    backup_file_name_with_extension, backup_temp_path, cleanup_backup_temps,
    cleanup_restore_candidates, create_backup_file, create_backup_file_verified,
    create_backup_staging, execute_restore, finalize_restore, program_schema_version, prune_plan,
    prune_plan_for_kind, read_restore_journal, recover_or_apply_pending_restore, request_restore,
    required_space_bytes, restore_journal_path, rollback_scene, validate_candidate,
    write_restore_journal, BackupRecord, CandidateReport, RestoreFault, RestoreJournal,
    RestoreReport, RestoreRequest, RestoreStartupOutcome, RestoreStep, RestoreStepRecord,
    SceneProtection, BACKUP_RETENTION, BACKUP_TEMP_INFIX, ENCRYPTED_BACKUP_EXTENSION,
    PRE_RESTORE_INFIX, RESTORE_CANDIDATE_INFIX, RESTORE_JOURNAL_SUFFIX, RESTORE_TEMP_INFIX,
    SPACE_MARGIN_DENOMINATOR, SPACE_MARGIN_NUMERATOR,
};

pub use checkpoint::{
    checkpoint_truncate_once, checkpoint_truncate_with_backoff, CheckpointAttempt,
    CheckpointConfig, CheckpointReport, CHECKPOINT_INITIAL_BACKOFF, CHECKPOINT_MAX_ATTEMPTS,
    CHECKPOINT_MAX_BACKOFF, WAL_FORCE_CHECKPOINT_BYTES,
};
pub use error::StoreError;
pub use migration::{
    checksum, migrate, migrate_with, AppliedMigration, MigrationFile, EMBEDDED_MIGRATIONS,
    MIGRATIONS_TABLE,
};
pub use ops::{
    ArtifactRecord, AuditLogRecord, AuditQuery, AuditQueryResult, PermissionGrantRecord,
    PermissionRecord, PermissionRuleRecord, ProviderModelRecord, ProviderRecord, SessionQuery,
    SessionSearchHit, StoreCommand, StoreOutcome, StoreSummary, WorkspaceSessionCount,
    WorkspaceSessionCounts, AUDIT_QUERY_DEFAULT_LIMIT, AUDIT_QUERY_MAX_LIMIT, SUMMARY_TABLES,
};
pub use pragma::PragmaSnapshot;
pub use preflight::{preflight, PreflightReport};
pub use store::{quick_check, ExportReport, IntegrityReport, Store, StoreMode, TableExport};
pub use write_queue::{
    BatchTrigger, CommitReceipt, QueueMetrics, QueuePressureLevel, QueuePressurePhase, ReadPool,
    ReadPoolCloseReport, ShutdownConfig, ShutdownReport, ShutdownStep, ShutdownStepRecord,
    StoreRuntime, WriteQueue, WriteQueueAlert, WriteQueueConfig, FLUSH_INTERVAL, L1_THRESHOLD,
    L2_THRESHOLD, MAX_BATCH_ENTRIES, QUEUE_CAPACITY, READ_CONNECTION_COUNT, SHUTDOWN_DRAIN_TIMEOUT,
    SHUTDOWN_READ_CLOSE_TIMEOUT,
};

#[cfg(test)]
mod tests {
    #[test]
    fn store_depends_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
    }
}
