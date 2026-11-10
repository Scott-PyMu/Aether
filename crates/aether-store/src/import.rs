//! 导入（M10-04 / ADR-034）：JSONL 格式校验、staging DB、ID 重映射、原子切换。
//!
//! 口径（ADR-034 §2 D-034-1～D-034-5）：
//! - 导入源复用归档 v1 JSONL（首行 manifest：`kind=manifest`、`v=1`、`rows`/`sha256`）；
//! - 流式读取，单行 ≤2 MiB、manifest ≤64 KiB、文件 ≤20 GiB；
//! - staging 使用 `VACUUM INTO` 生成当前库快照，导入行批量写入（≤1000 行/事务）；
//! - 原子切换复用 D13 恢复引擎（`execute_restore`），现场保护后缀为 `.pre-import-<ts>`；
//! - `remap`（默认）为 session/message/run/event/audit 生成新 ULID；`preserve` 冲突显式拒绝；
//! - `client_msg_id` 同会话去重；`events.seq`/`messages.seq` 按会话重排；
//! - 失败/取消不改变既有 `aether.db`，staging 可清理；`kill -9` 后不自动续跑。
//!
//! 本模块不依赖 `aether-app`/`aether-security`（AGENTS §2.1）；不执行导入内容、不访问网络。

use std::fs::{self, File};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_core::EventType;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::backup::{
    execute_restore, finalize_restore, program_schema_version, required_space_bytes, RestoreFault,
    RestoreRequest,
};
use crate::error::StoreError;
use crate::migration;
use crate::pragma;
use crate::store::quick_check;

/// 导入工作目录（`<data_dir>/imports`）。
pub const IMPORT_DIR: &str = "imports";
/// 单作业状态文件。
pub const IMPORT_JOB_FILE: &str = "job.json";
/// 待重启切换标记（只允许一个）。
pub const IMPORT_PENDING_FILE: &str = "pending.json";
/// staging 数据库文件名。
pub const IMPORT_WORK_DB: &str = "work.db";
/// 导入现场保护后缀（ADR-034 D-034-2）。
pub const PRE_IMPORT_INFIX: &str = ".pre-import-";
/// 作业/状态文件格式版本。
pub const IMPORT_JOB_VERSION: u32 = 1;
/// 导入源格式版本（归档 v1）。
pub const IMPORT_FORMAT_VERSION: u32 = 1;
/// 文件上限：20 GiB（ADR-034 D-034-1）。
pub const IMPORT_FILE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// 单行上限：2 MiB（ADR-034 D-034-1）。
pub const IMPORT_LINE_MAX_BYTES: usize = 2 * 1024 * 1024;
/// manifest 行上限：64 KiB（ADR-034 D-034-1）。
pub const IMPORT_MANIFEST_LINE_MAX_BYTES: usize = 64 * 1024;
/// 流式缓冲：1 MiB（ADR-034 D-034-1）。
pub const IMPORT_BUFFER_BYTES: usize = 1024 * 1024;
/// 每批行数（ADR-034 D-034-2）。
pub const IMPORT_BATCH_ROWS: usize = 1000;
/// staging/残留清理窗口（ADR-034 D-034-2）。
pub const IMPORT_STALE_HOURS: i64 = 24;
/// 导入源：单会话导出。
pub const IMPORT_SOURCE_SESSION_EXPORT: &str = "session-export";
/// 导入源：全库归档导出。
pub const IMPORT_SOURCE_ARCHIVE_EXPORT: &str = "archive-export";
/// 导入源：保留清理自动归档。
pub const IMPORT_SOURCE_AUTO_RETENTION: &str = "auto-retention";
/// 审计/导出源：审计导出（仅解析支持；M10-04 命令不接受该源）。
pub const IMPORT_SOURCE_AUDIT_EXPORT: &str = "audit-export";
/// `session_import` 接受的源（ADR-034 D-034-1）。
pub const SESSION_IMPORT_SOURCES: [&str; 1] = [IMPORT_SOURCE_SESSION_EXPORT];
/// `archive_import` 接受的源（ADR-034 D-034-1）。
pub const ARCHIVE_IMPORT_SOURCES: [&str; 2] =
    [IMPORT_SOURCE_ARCHIVE_EXPORT, IMPORT_SOURCE_AUTO_RETENTION];
/// 冲突列表上限（错误 details）。
pub const IMPORT_CONFLICT_MAX: usize = 50;
/// 警告列表上限（`import_status`）。
pub const IMPORT_WARNING_MAX: usize = 100;
/// 审计动作（ADR-034 D-034-5）。
pub const AUDIT_ACTION_IMPORT_STARTED: &str = "import.started";
/// 审计动作：完成。
pub const AUDIT_ACTION_IMPORT_COMPLETED: &str = "import.completed";
/// 审计动作：失败。
pub const AUDIT_ACTION_IMPORT_FAILED: &str = "import.failed";
/// 审计动作：取消。
pub const AUDIT_ACTION_IMPORT_CANCELLED: &str = "import.cancelled";

/// 导入模式（ADR-034 D-034-3）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// 保留原 ID；冲突显式拒绝（非默认）。
    Preserve,
    /// 生成新 ULID 并重映射全部引用（默认）。
    #[default]
    Remap,
}

impl ImportMode {
    /// 线上取值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Remap => "remap",
        }
    }
}

/// 导入作业状态（ADR-034 D-034-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportJobStatus {
    /// 已受理（路径/首行预检通过，异步校验尚未开始）。
    Staging,
    /// 流式校验源文件（行/哈希/字段）。
    Validating,
    /// 正在生成 staging/预览（dry-run）或切换（启动时）。
    Applying,
    /// staging 已完成，等待重启后原子切换。
    AwaitingRestart,
    /// 完成（真实导入在重启后完成；dry-run 在预览后完成）。
    Completed,
    /// 失败（既有数据不变）。
    Failed,
    /// 取消（既有数据不变）。
    Cancelled,
}

impl ImportJobStatus {
    /// 线上取值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Validating => "validating",
            Self::Applying => "applying",
            Self::AwaitingRestart => "awaiting_restart",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否活动（维护窗口/可取消）。
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Staging | Self::Validating | Self::Applying | Self::AwaitingRestart
        )
    }
}

/// 导入失败类别（映射到 `business_code`；不扩张 `IpcErrorCode` 32 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFailureKind {
    /// 格式/版本/字段非法。
    InvalidFormat,
    /// manifest 行数/哈希不一致。
    ChecksumMismatch,
    /// `preserve` 模式 ID 冲突。
    Conflict,
    /// 文件/行/空间超限。
    TooLarge,
    /// 已取消。
    Cancelled,
    /// 作业不存在。
    JobNotFound,
    /// 作业不在活动状态。
    JobNotActive,
    /// IO/SQLite/内部错误。
    Internal,
}

impl ImportFailureKind {
    /// 线上业务码（`error.data.business_code`）。
    pub const fn business_code(self) -> &'static str {
        match self {
            Self::InvalidFormat => "import_invalid_format",
            Self::ChecksumMismatch => "import_checksum_mismatch",
            Self::Conflict => "import_conflict",
            Self::TooLarge => "import_too_large",
            Self::Cancelled => "import_cancelled",
            Self::JobNotFound => "import_job_not_found",
            Self::JobNotActive => "import_job_not_active",
            Self::Internal => "internal",
        }
    }
}

/// 导入失败（结构化；`details` 进入 `error.data.details`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportFailure {
    pub kind: ImportFailureKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ImportFailure {
    pub fn new(kind: ImportFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn invalid_format(message: impl Into<String>) -> Self {
        Self::new(ImportFailureKind::InvalidFormat, message)
    }

    pub fn checksum_mismatch(message: impl Into<String>) -> Self {
        Self::new(ImportFailureKind::ChecksumMismatch, message)
    }

    pub fn conflict(message: impl Into<String>, id: &str, kind: &str) -> Self {
        let details = json!({"conflicts": [{"id": id, "kind": kind}]});
        Self::new(ImportFailureKind::Conflict, message).with_details(details)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(ImportFailureKind::TooLarge, message)
    }

    pub fn cancelled() -> Self {
        Self::new(ImportFailureKind::Cancelled, "导入已取消（既有数据未改变）")
    }

    pub fn job_not_found(job_id: &str) -> Self {
        Self::new(
            ImportFailureKind::JobNotFound,
            format!("导入作业不存在（{job_id}）"),
        )
    }

    pub fn job_not_active(job_id: &str, status: ImportJobStatus) -> Self {
        Self::new(
            ImportFailureKind::JobNotActive,
            format!(
                "导入作业不在活动状态（{job_id}，status={}）；既有数据未改变",
                status.as_str()
            ),
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ImportFailureKind::Internal, message)
    }

    pub fn business_code(&self) -> &'static str {
        self.kind.business_code()
    }
}

impl std::fmt::Display for ImportFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}（{}）", self.message, self.kind.business_code())
    }
}

impl std::error::Error for ImportFailure {}

impl From<StoreError> for ImportFailure {
    fn from(error: StoreError) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<rusqlite::Error> for ImportFailure {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(format!("SQLite 错误：{error}"))
    }
}

impl From<std::io::Error> for ImportFailure {
    fn from(error: std::io::Error) -> Self {
        Self::internal(format!("文件系统错误：{error}"))
    }
}

impl From<serde_json::Error> for ImportFailure {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid_format(format!("JSON 解析失败：{error}"))
    }
}

/// 导入 manifest（首行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportManifest {
    pub v: u32,
    pub kind: String,
    #[serde(default)]
    pub created_at: Option<i64>,
    pub source: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub rows: u64,
    pub sha256: String,
    #[serde(default)]
    pub exported_by: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
}

/// 导入计数（按 kind）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    pub sessions: u64,
    pub messages: u64,
    pub events: u64,
    pub runs: u64,
    pub audits: u64,
    /// `client_msg_id` 重复跳过数。
    pub duplicates: u64,
    /// 未知会话/未知事件类型等跳过数。
    pub skipped: u64,
}

impl ImportSummary {
    fn add_kind(&mut self, kind: &str) {
        match kind {
            "session" => self.sessions = self.sessions.saturating_add(1),
            "message" => self.messages = self.messages.saturating_add(1),
            "event" => self.events = self.events.saturating_add(1),
            "run" => self.runs = self.runs.saturating_add(1),
            "audit" => self.audits = self.audits.saturating_add(1),
            _ => {}
        }
    }

}

/// 进度（`import_status`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProgress {
    pub rows: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// 导入警告（`path` 为 `line:<n>`，不记录文件内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportWarning {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub detail: String,
}

/// 作业错误（`code` = 业务码）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportFailureInfo {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// 持久化导入作业（`imports/<job_id>/job.json`；不含文件内容/密钥）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportJob {
    pub v: u32,
    pub job_id: String,
    /// `session` | `archive`。
    pub kind: String,
    pub status: ImportJobStatus,
    pub mode: ImportMode,
    pub dry_run: bool,
    /// canonicalize 后的源文件路径（诊断；不进入事件）。
    pub source_path: String,
    pub source_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    pub progress: ImportProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ImportSummary>,
    #[serde(default)]
    pub warnings: Vec<ImportWarning>,
    #[serde(default)]
    pub warnings_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ImportFailureInfo>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 待重启切换标记。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingImport {
    pub v: u32,
    pub job_id: String,
    pub created_at: i64,
}

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportValidation {
    pub manifest: ImportManifest,
    pub rows: u64,
    pub bytes: u64,
    pub sha256: String,
    pub summary: ImportSummary,
    pub warnings: Vec<ImportWarning>,
    pub warnings_truncated: bool,
}

/// staging 应用结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportApplyReport {
    pub summary: ImportSummary,
    pub rows: u64,
    pub bytes: u64,
    pub sha256: String,
    pub duration_ms: u64,
    pub warnings: Vec<ImportWarning>,
    pub warnings_truncated: bool,
}

/// staging 验收结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingVerification {
    pub bytes: u64,
    pub schema_version: i64,
    pub foreign_key_violations: usize,
}

/// 启动切换结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBootReport {
    pub job_id: String,
    pub kind: String,
    pub mode: ImportMode,
    pub summary: ImportSummary,
    pub already_applied: bool,
    pub duration_ms: u64,
}

struct ImportScan {
    manifest: ImportManifest,
    rows: u64,
    bytes: u64,
    sha256: String,
}

struct WarningSink {
    warnings: Vec<ImportWarning>,
    truncated: bool,
}

impl WarningSink {
    fn new() -> Self {
        Self {
            warnings: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, code: &str, line: u64, detail: impl Into<String>) {
        if self.warnings.len() >= IMPORT_WARNING_MAX {
            self.truncated = true;
            return;
        }
        self.warnings.push(ImportWarning {
            code: code.to_owned(),
            path: Some(format!("line:{line}")),
            detail: detail.into(),
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowDisposition {
    Include,
    Skip,
}

/// 导入工作根目录。
pub fn import_root(data_dir: &Path) -> PathBuf {
    data_dir.join(IMPORT_DIR)
}

/// 作业目录。
pub fn job_dir(data_dir: &Path, job_id: &str) -> PathBuf {
    import_root(data_dir).join(job_id)
}

/// 作业状态文件。
pub fn job_path(data_dir: &Path, job_id: &str) -> PathBuf {
    job_dir(data_dir, job_id).join(IMPORT_JOB_FILE)
}

/// 待切换标记文件。
pub fn pending_path(data_dir: &Path) -> PathBuf {
    import_root(data_dir).join(IMPORT_PENDING_FILE)
}

/// staging 数据库路径。
pub fn work_db_path(data_dir: &Path, job_id: &str) -> PathBuf {
    job_dir(data_dir, job_id).join(IMPORT_WORK_DB)
}

/// 作业 id 是否可作为目录名（ULID；拒绝路径分隔符）。
pub fn is_valid_job_id(job_id: &str) -> bool {
    ulid::Ulid::from_str(job_id).is_ok()
}

/// 生成新 ULID。
pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ImportFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)?;
    fs::write(&temp, text)?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// 写作业状态（原子写；不含内容/密钥）。
pub fn write_job(data_dir: &Path, job: &ImportJob) -> Result<(), ImportFailure> {
    write_json_atomic(&job_path(data_dir, &job.job_id), job)
}

/// 读作业状态；不存在 → `None`。
pub fn read_job(data_dir: &Path, job_id: &str) -> Result<Option<ImportJob>, ImportFailure> {
    if !is_valid_job_id(job_id) {
        return Ok(None);
    }
    let path = job_path(data_dir, job_id);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let job: ImportJob = serde_json::from_str(&text)
        .map_err(|error| ImportFailure::internal(format!("导入作业状态损坏：{error}")))?;
    Ok(Some(job))
}

/// 写待切换标记（仅 `awaiting_restart`）。
pub fn write_pending(data_dir: &Path, job_id: &str, at_ms: i64) -> Result<(), ImportFailure> {
    let pending = PendingImport {
        v: IMPORT_JOB_VERSION,
        job_id: job_id.to_owned(),
        created_at: at_ms,
    };
    write_json_atomic(&pending_path(data_dir), &pending)
}

/// 读待切换标记；不存在/非法 → `None`（非法由调用方清理）。
pub fn read_pending(data_dir: &Path) -> Result<Option<PendingImport>, ImportFailure> {
    let path = pending_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    match serde_json::from_str::<PendingImport>(&text) {
        Ok(pending) if is_valid_job_id(&pending.job_id) => Ok(Some(pending)),
        _ => Ok(None),
    }
}

/// 删除待切换标记（不存在 → 幂等）。
pub fn remove_pending(data_dir: &Path) -> Result<(), ImportFailure> {
    let path = pending_path(data_dir);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// 是否存在待重启切换。
pub fn pending_exists(data_dir: &Path) -> bool {
    read_pending(data_dir).ok().flatten().is_some()
}

/// 是否存在活动导入作业（pending 或近期 staging/validating/applying）。
pub fn has_active_job(data_dir: &Path, at_ms: i64) -> bool {
    if pending_exists(data_dir) {
        return true;
    }
    let root = import_root(data_dir);
    let Ok(entries) = fs::read_dir(&root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(job_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(Some(job)) = read_job(data_dir, job_id) else {
            continue;
        };
        if job.status.is_active() && at_ms.saturating_sub(job.updated_at) < IMPORT_STALE_HOURS * 3_600_000
        {
            return true;
        }
    }
    false
}

fn remove_db_files(path: &Path) -> Result<(), ImportFailure> {
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "aether.db".to_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

/// 清理 staging 残留（`work.db` 与侧车；保留 `job.json`）。
pub fn cleanup_staging(data_dir: &Path, job_id: &str) -> Result<(), ImportFailure> {
    if !is_valid_job_id(job_id) {
        return Ok(());
    }
    remove_db_files(&work_db_path(data_dir, job_id))
}

/// 启动时收口被 kill 的进行中作业（无 `pending` 的 staging/validating/applying）。
///
/// 不自动续跑；标记 `failed`、清理 staging，既有库不变。返回收口数量。
pub fn reconcile_interrupted_imports(data_dir: &Path, at_ms: i64) -> Result<usize, ImportFailure> {
    let pending = read_pending(data_dir).ok().flatten();
    let pending_id = pending.as_ref().map(|pending| pending.job_id.as_str());
    let root = import_root(data_dir);
    let mut repaired = 0usize;
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(job_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(Some(mut job)) = read_job(data_dir, job_id) else {
            continue;
        };
        if Some(job_id) == pending_id {
            continue;
        }
        if matches!(
            job.status,
            ImportJobStatus::Staging
                | ImportJobStatus::Validating
                | ImportJobStatus::Applying
                | ImportJobStatus::AwaitingRestart
        ) {
            job.status = ImportJobStatus::Failed;
            job.error = Some(ImportFailureInfo {
                code: "import_interrupted".to_owned(),
                message: "上次导入在完成前被中断（daemon 退出/kill）；已清理 staging，不自动续跑"
                    .to_owned(),
                details: None,
            });
            job.updated_at = at_ms;
            if write_job(data_dir, &job).is_ok() {
                let _ = cleanup_staging(data_dir, job_id);
                repaired = repaired.saturating_add(1);
            }
        }
    }
    Ok(repaired)
}

/// 清理 >24h 的导入残留（作业目录、staging、`.pre-import-*`）。
///
/// 待切换作业保留；终态作业保留 `job.json` 24h 供 `import_status` 查询。
pub fn cleanup_imports(data_dir: &Path, at_ms: i64) -> Result<usize, ImportFailure> {
    let stale_before = at_ms.saturating_sub(IMPORT_STALE_HOURS * 3_600_000);
    let pending = read_pending(data_dir).ok().flatten();
    let pending_id = pending.as_ref().map(|pending| pending.job_id.as_str());
    let root = import_root(data_dir);
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(job_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if Some(job_id) == pending_id {
                continue;
            }
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            let recent = modified >= stale_before;
            let job = read_job(data_dir, job_id).ok().flatten();
            if !recent {
                if fs::remove_dir_all(&path).is_ok() {
                    removed = removed.saturating_add(1);
                }
            } else if job.as_ref().is_some_and(|job| !job.status.is_active()) {
                // 终态作业：staging 可立即清理；状态文件保留 24h。
                if cleanup_staging(data_dir, job_id).is_ok() {
                    removed = removed.saturating_add(1);
                }
            }
        }
    }
    // `.pre-import-*` 现场保护文件 >24h 清理。
    if let Ok(entries) = fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains(PRE_IMPORT_INFIX) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            if modified < stale_before && fs::remove_file(entry.path()).is_ok() {
                removed = removed.saturating_add(1);
            }
        }
    }
    Ok(removed)
}

fn read_line_limited(reader: &mut impl BufRead, max: usize) -> Result<Option<Vec<u8>>, ImportFailure> {
    let mut buffer = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        buffer.extend_from_slice(&available[..take]);
        let finished = newline.is_some();
        reader.consume(take);
        if buffer.len() > max.saturating_add(2) {
            return Err(ImportFailure::too_large(format!(
                "导入行超过上限（{max} 字节）"
            )));
        }
        if finished {
            break;
        }
    }
    Ok(Some(buffer))
}

fn trim_line_end(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn parse_manifest(line: &[u8], expected_sources: &[&str]) -> Result<ImportManifest, ImportFailure> {
    let line = trim_line_end(line);
    if line.is_empty() {
        return Err(ImportFailure::invalid_format("导入文件为空（缺少 manifest 首行）"));
    }
    let text = std::str::from_utf8(line)
        .map_err(|_| ImportFailure::invalid_format("manifest 不是合法 UTF-8"))?;
    let manifest: ImportManifest = serde_json::from_str(text)?;
    if manifest.kind != "manifest" {
        return Err(ImportFailure::invalid_format(format!(
            "首行必须是 manifest（实际 kind={}）",
            manifest.kind
        )));
    }
    if manifest.v > IMPORT_FORMAT_VERSION {
        return Err(ImportFailure::invalid_format(format!(
            "导入格式版本 v{} 高于当前支持 v{}；请升级应用",
            manifest.v, IMPORT_FORMAT_VERSION
        )));
    }
    if manifest.v < IMPORT_FORMAT_VERSION {
        return Err(ImportFailure::invalid_format(format!(
            "导入格式版本 v{} 不受支持（当前 v{}）",
            manifest.v, IMPORT_FORMAT_VERSION
        )));
    }
    if !expected_sources.contains(&manifest.source.as_str()) {
        return Err(ImportFailure::invalid_format(format!(
            "导入源 {} 与命令不匹配（允许：{}）",
            manifest.source,
            expected_sources.join("/")
        )));
    }
    if manifest.source == IMPORT_SOURCE_SESSION_EXPORT
        && manifest.session_id.as_deref().is_none_or(str::is_empty)
    {
        return Err(ImportFailure::invalid_format(
            "session-export 的 manifest 必须包含 session_id",
        ));
    }
    if manifest.sha256.len() != 64
        || !manifest
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ImportFailure::invalid_format(
            "manifest.sha256 必须是 64 位小写十六进制",
        ));
    }
    Ok(manifest)
}

/// 预检：文件存在/大小/首行 manifest（`session_import`/`archive_import` 受理前）。
pub fn precheck_source(
    source_path: &Path,
    expected_sources: &[&str],
) -> Result<ImportManifest, ImportFailure> {
    let metadata = fs::metadata(source_path)?;
    if !metadata.is_file() {
        return Err(ImportFailure::invalid_format("导入源必须是文件"));
    }
    if metadata.len() > IMPORT_FILE_MAX_BYTES {
        return Err(ImportFailure::too_large(format!(
            "导入文件 {} 字节超过上限 {} 字节",
            metadata.len(),
            IMPORT_FILE_MAX_BYTES
        )));
    }
    let file = File::open(source_path)?;
    let mut reader = std::io::BufReader::with_capacity(IMPORT_BUFFER_BYTES, file);
    let first = read_line_limited(&mut reader, IMPORT_MANIFEST_LINE_MAX_BYTES)?
        .ok_or_else(|| ImportFailure::invalid_format("导入文件为空（缺少 manifest 首行）"))?;
    parse_manifest(&first, expected_sources)
}

fn scan_source(
    source_path: &Path,
    expected_sources: &[&str],
    mut on_row: impl FnMut(&str, &Value, u64) -> Result<(), ImportFailure>,
    progress: &mut dyn FnMut(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportScan, ImportFailure> {
    let metadata = fs::metadata(source_path)?;
    if !metadata.is_file() {
        return Err(ImportFailure::invalid_format("导入源必须是文件"));
    }
    if metadata.len() > IMPORT_FILE_MAX_BYTES {
        return Err(ImportFailure::too_large(format!(
            "导入文件 {} 字节超过上限 {} 字节",
            metadata.len(),
            IMPORT_FILE_MAX_BYTES
        )));
    }
    let file = File::open(source_path)?;
    let mut reader = std::io::BufReader::with_capacity(IMPORT_BUFFER_BYTES, file);
    let first = read_line_limited(&mut reader, IMPORT_MANIFEST_LINE_MAX_BYTES)?
        .ok_or_else(|| ImportFailure::invalid_format("导入文件为空（缺少 manifest 首行）"))?;
    let manifest = parse_manifest(&first, expected_sources)?;
    let mut hasher = Sha256::new();
    let mut rows = 0u64;
    let mut bytes = first.len() as u64;
    loop {
        if cancelled() {
            return Err(ImportFailure::cancelled());
        }
        let Some(raw) = read_line_limited(&mut reader, IMPORT_LINE_MAX_BYTES)? else {
            break;
        };
        bytes = bytes.saturating_add(raw.len() as u64);
        let line = trim_line_end(&raw);
        if line.is_empty() {
            return Err(ImportFailure::invalid_format(format!(
                "第 {} 行为空行（payload 行不得为空）",
                rows + 1
            )));
        }
        if line.len() > IMPORT_LINE_MAX_BYTES {
            return Err(ImportFailure::too_large(format!(
                "第 {} 行超过上限 {} 字节",
                rows + 1,
                IMPORT_LINE_MAX_BYTES
            )));
        }
        rows = rows.saturating_add(1);
        hasher.update(line);
        hasher.update(b"\n");
        let text = std::str::from_utf8(line)
            .map_err(|_| ImportFailure::invalid_format(format!("第 {rows} 行不是合法 UTF-8")))?;
        let value: Value = serde_json::from_str(text)
            .map_err(|error| ImportFailure::invalid_format(format!("第 {rows} 行 JSON 非法：{error}")))?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ImportFailure::invalid_format(format!("第 {rows} 行缺少 kind")))?;
        if crate::archive::archive_line_whitelist(kind).is_none() {
            return Err(ImportFailure::invalid_format(format!(
                "第 {rows} 行 kind={kind} 未登记（拒绝未知 kind，不猜测语义）"
            )));
        }
        on_row(kind, &value, rows)?;
        progress(bytes, rows);
    }
    let sha256 = hex::encode(hasher.finalize());
    if rows != manifest.rows || sha256 != manifest.sha256 {
        return Err(ImportFailure::checksum_mismatch(format!(
            "manifest 行数/哈希与实际 payload 不一致（manifest rows={} sha256={}；实际 rows={} sha256={}）",
            manifest.rows, manifest.sha256, rows, sha256
        )));
    }
    Ok(ImportScan {
        manifest,
        rows,
        bytes,
        sha256,
    })
}

fn invalid_detail(kind: &str, line: u64, field: &str, message: impl Into<String>) -> ImportFailure {
    ImportFailure::invalid_format(format!("第 {line} 行 {kind}.{field} 非法：{}", message.into()))
        .with_details(json!({"line": line, "kind": kind, "field": field}))
}

fn required_str<'a>(
    value: &'a Value,
    field: &str,
    kind: &str,
    line: u64,
) -> Result<&'a str, ImportFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid_detail(kind, line, field, "缺少非空字符串"))
}

fn optional_str<'a>(
    value: &'a Value,
    field: &str,
    kind: &str,
    line: u64,
) -> Result<Option<&'a str>, ImportFailure> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text)),
        Some(_) => Err(invalid_detail(kind, line, field, "必须是非空字符串或 null")),
    }
}

fn required_i64(value: &Value, field: &str, kind: &str, line: u64) -> Result<i64, ImportFailure> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid_detail(kind, line, field, "缺少整数"))
}

fn optional_i64(
    value: &Value,
    field: &str,
    kind: &str,
    line: u64,
) -> Result<Option<i64>, ImportFailure> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| invalid_detail(kind, line, field, "必须是整数或 null")),
        Some(_) => Err(invalid_detail(kind, line, field, "必须是整数或 null")),
    }
}

fn optional_object_text(
    value: &Value,
    field: &str,
    kind: &str,
    line: u64,
) -> Result<Option<String>, ImportFailure> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(_)) => Ok(Some(serde_json::to_string(
            value.get(field).unwrap_or(&Value::Null),
        )?)),
        Some(_) => Err(invalid_detail(kind, line, field, "必须是 JSON 对象或 null")),
    }
}

fn check_line_version(value: &Value, line: u64) -> Result<(), ImportFailure> {
    if let Some(version) = value.get("v") {
        let version = version
            .as_i64()
            .ok_or_else(|| invalid_detail("line", line, "v", "必须是整数"))?;
        if version != i64::from(IMPORT_FORMAT_VERSION) {
            return Err(ImportFailure::invalid_format(format!(
                "第 {line} 行版本 v{version} 不受支持（当前 v{IMPORT_FORMAT_VERSION}）"
            )));
        }
    }
    Ok(())
}

const SESSION_STATUSES: [&str; 8] = [
    "creating",
    "idle",
    "running",
    "paused",
    "waiting_permission",
    "completed",
    "failed",
    "cancelled",
];
const MESSAGE_ROLES: [&str; 4] = ["user", "assistant", "system", "tool"];
const RUN_STATUSES: [&str; 6] = ["queued", "running", "succeeded", "failed", "cancelled", "timeout"];
const AUDIT_ACTORS: [&str; 3] = ["user", "agent", "system"];
const SECRET_FIELD_NAMES: [&str; 10] = [
    "api_key",
    "apikey",
    "token",
    "access_token",
    "refresh_token",
    "password",
    "secret",
    "client_secret",
    "private_key",
    "authorization",
];

fn reject_secret_fields(value: &Value, line: u64) -> Result<(), ImportFailure> {
    fn walk(value: &Value, line: u64, depth: usize) -> Result<(), ImportFailure> {
        if depth > 16 {
            return Err(ImportFailure::invalid_format(format!(
                "第 {line} 行嵌套过深（>16）"
            )));
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let normalized = key.to_ascii_lowercase();
                    if SECRET_FIELD_NAMES.contains(&normalized.as_str()) {
                        return Err(ImportFailure::invalid_format(format!(
                            "第 {line} 行包含密钥字段 {key}；拒绝导入（D10）"
                        )));
                    }
                    walk(child, line, depth + 1)?;
                }
                Ok(())
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, line, depth + 1)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(value, line, 0)
}

fn check_unknown_fields(
    value: &Value,
    kind: &str,
    line: u64,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_detail(kind, line, "kind", "行必须是 JSON 对象"))?;
    let mut whitelist: Vec<&str> = crate::archive::archive_line_whitelist(kind)
        .unwrap_or(&[])
        .to_vec();
    if kind == "message" {
        whitelist.push("client_msg_id");
    }
    for key in object.keys() {
        if !whitelist.contains(&key.as_str()) {
            warnings.push("unknown_field", line, format!("忽略未知字段 {key}（{kind}）"));
        }
    }
    Ok(())
}

fn validate_row(
    kind: &str,
    value: &Value,
    line: u64,
    warnings: &mut WarningSink,
) -> Result<RowDisposition, ImportFailure> {
    if !value.is_object() {
        return Err(invalid_detail(kind, line, "kind", "行必须是 JSON 对象"));
    }
    reject_secret_fields(value, line)?;
    check_unknown_fields(value, kind, line, warnings)?;
    check_line_version(value, line)?;
    match kind {
        "session" => {
            let _id = required_str(value, "id", kind, line)?;
            let _runtime = required_str(value, "runtime_id", kind, line)?;
            let status = required_str(value, "status", kind, line)?;
            if !SESSION_STATUSES.contains(&status) {
                return Err(invalid_detail(kind, line, "status", "不在会话状态白名单"));
            }
            let _created = required_i64(value, "created_at", kind, line)?;
            let _updated = required_i64(value, "updated_at", kind, line)?;
            let _title = optional_str(value, "title", kind, line)?;
            let _model = optional_str(value, "model", kind, line)?;
            let _workspace = optional_str(value, "workspace_id", kind, line)?;
            let _parent = optional_str(value, "parent_session_id", kind, line)?;
            let _system_prompt = optional_str(value, "system_prompt", kind, line)?;
            let _config = optional_object_text(value, "config", kind, line)?;
            let _token_usage = optional_object_text(value, "token_usage", kind, line)?;
            let _closed = optional_i64(value, "closed_at", kind, line)?;
            if let Some(depth) = optional_i64(value, "thinking_depth", kind, line)? {
                if !(0..=4).contains(&depth) {
                    return Err(invalid_detail(kind, line, "thinking_depth", "必须在 0..=4"));
                }
            }
        }
        "message" => {
            let _id = required_str(value, "id", kind, line)?;
            let _session = required_str(value, "session_id", kind, line)?;
            let role = required_str(value, "role", kind, line)?;
            if !MESSAGE_ROLES.contains(&role) {
                return Err(invalid_detail(kind, line, "role", "不在消息角色白名单"));
            }
            let _content = value
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_detail(kind, line, "content", "缺少字符串"))?;
            let seq = required_i64(value, "seq", kind, line)?;
            if seq < 0 {
                return Err(invalid_detail(kind, line, "seq", "必须 >= 0"));
            }
            let _ts = required_i64(value, "ts", kind, line)?;
            if let Some(client_msg_id) = optional_str(value, "client_msg_id", kind, line)? {
                if client_msg_id.chars().count() > 128 {
                    return Err(invalid_detail(
                        kind,
                        line,
                        "client_msg_id",
                        "超过 128 字符",
                    ));
                }
            }
        }
        "event" => {
            let _id = required_str(value, "id", kind, line)?;
            let _session = required_str(value, "session_id", kind, line)?;
            let _runtime = required_str(value, "runtime_id", kind, line)?;
            let event_type = required_str(value, "type", kind, line)?;
            let seq = required_i64(value, "seq", kind, line)?;
            if seq < 0 {
                return Err(invalid_detail(kind, line, "seq", "必须 >= 0"));
            }
            let _ts = required_i64(value, "ts", kind, line)?;
            let version = required_i64(value, "v", kind, line)?;
            if version != i64::from(IMPORT_FORMAT_VERSION) {
                return Err(ImportFailure::invalid_format(format!(
                    "第 {line} 行 event.v={version} 不受支持"
                )));
            }
            if value.get("payload").and_then(Value::as_object).is_none() {
                return Err(invalid_detail(kind, line, "payload", "必须是 JSON 对象"));
            }
            let _run = optional_str(value, "run_id", kind, line)?;
            if EventType::from_str(event_type).is_err() {
                warnings.push(
                    "unknown_event_type",
                    line,
                    format!("跳过未登记事件类型 {event_type}（不执行、不广播）"),
                );
                return Ok(RowDisposition::Skip);
            }
        }
        "run" => {
            let _id = required_str(value, "id", kind, line)?;
            let _session = required_str(value, "session_id", kind, line)?;
            let status = required_str(value, "status", kind, line)?;
            if !RUN_STATUSES.contains(&status) {
                return Err(invalid_detail(kind, line, "status", "不在 run 状态白名单"));
            }
            let _started = required_i64(value, "started_at", kind, line)?;
            let _finished = optional_i64(value, "finished_at", kind, line)?;
            let _input = optional_str(value, "input_message_id", kind, line)?;
            let _error = optional_str(value, "error", kind, line)?;
            if let Some(depth) = optional_i64(value, "thinking_depth", kind, line)? {
                if !(0..=4).contains(&depth) {
                    return Err(invalid_detail(kind, line, "thinking_depth", "必须在 0..=4"));
                }
            }
        }
        "audit" => {
            let _id = required_str(value, "id", kind, line)?;
            let actor = required_str(value, "actor", kind, line)?;
            if !AUDIT_ACTORS.contains(&actor) {
                return Err(invalid_detail(kind, line, "actor", "不在审计 actor 白名单"));
            }
            let action = required_str(value, "action", kind, line)?;
            if action.chars().count() > 64 {
                return Err(invalid_detail(kind, line, "action", "超过 64 字符"));
            }
            let _ts = required_i64(value, "ts", kind, line)?;
            let _session = optional_str(value, "session_id", kind, line)?;
            let _runtime = optional_str(value, "runtime_id", kind, line)?;
            let _resource = optional_str(value, "resource", kind, line)?;
            let _result = optional_str(value, "result", kind, line)?;
        }
        _ => {
            return Err(ImportFailure::invalid_format(format!(
                "第 {line} 行 kind={kind} 未登记"
            )))
        }
    }
    Ok(RowDisposition::Include)
}

/// 流式校验导入源（不写任何数据库）。
pub fn validate_source(
    source_path: &Path,
    expected_sources: &[&str],
    progress: &mut dyn FnMut(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportValidation, ImportFailure> {
    let mut summary = ImportSummary::default();
    let mut warnings = WarningSink::new();
    let scan = scan_source(
        source_path,
        expected_sources,
        |kind, value, line| {
            match validate_row(kind, value, line, &mut warnings)? {
                RowDisposition::Include => summary.add_kind(kind),
                RowDisposition::Skip => summary.skipped = summary.skipped.saturating_add(1),
            }
            Ok(())
        },
        progress,
        cancelled,
    )?;
    Ok(ImportValidation {
        manifest: scan.manifest,
        rows: scan.rows,
        bytes: scan.bytes,
        sha256: scan.sha256,
        summary,
        warnings: warnings.warnings,
        warnings_truncated: warnings.truncated,
    })
}

fn table_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "session" => Some("sessions"),
        "message" => Some("messages"),
        "event" => Some("events"),
        "run" => Some("runs"),
        "audit" => Some("audit_log"),
        _ => None,
    }
}

fn id_exists(conn: &Connection, kind: &str, id: &str) -> Result<bool, ImportFailure> {
    let Some(table) = table_for_kind(kind) else {
        return Ok(false);
    };
    let sql = format!("SELECT 1 FROM {table} WHERE id = ?1 LIMIT 1");
    Ok(conn
        .query_row(&sql, [id], |_row| Ok(()))
        .optional()?
        .is_some())
}

fn mapped_id(
    conn: &Connection,
    kind: &str,
    old_id: &str,
) -> Result<Option<String>, ImportFailure> {
    conn.query_row(
        "SELECT new_id FROM import_id_map WHERE kind = ?1 AND old_id = ?2",
        params![kind, old_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(ImportFailure::from)
}

fn insert_mapping(
    conn: &Connection,
    kind: &str,
    old_id: &str,
    new_id: &str,
    session_old: Option<&str>,
    session_new: Option<&str>,
) -> Result<(), ImportFailure> {
    let duplicate: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM import_id_map WHERE kind = ?1 AND old_id = ?2",
            params![kind, old_id],
            |row| row.get(0),
        )
        .optional()?;
    if duplicate.is_some() {
        return Err(ImportFailure::invalid_format(format!(
            "导入文件包含重复 {kind} id={old_id}"
        )));
    }
    conn.execute(
        "INSERT INTO import_id_map (kind, old_id, new_id, session_old, session_new) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, old_id, new_id, session_old, session_new],
    )?;
    Ok(())
}

fn ensure_runtime(conn: &Connection, runtime_id: &str, at_ms: i64) -> Result<(), ImportFailure> {
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM runtimes WHERE id = ?1", [runtime_id], |row| {
            row.get(0)
        })
        .optional()?;
    if exists.is_none() {
        conn.execute(
            "INSERT OR IGNORE INTO runtimes \
             (id, name, kind, version, protocol, capabilities, endpoint, config, status, \
              status_reason, last_seen_at, created_at, updated_at, enabled) \
             VALUES (?1, ?1, 'imported', 'imported', '1.0', '[]', NULL, '{}', 'disabled', \
                     'imported_unknown_runtime', NULL, ?2, ?2, 0)",
            params![runtime_id, at_ms],
        )?;
    }
    Ok(())
}

fn workspace_exists(conn: &Connection, workspace_id: &str) -> Result<bool, ImportFailure> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM workspaces WHERE id = ?1 LIMIT 1",
            [workspace_id],
            |_row| Ok(()),
        )
        .optional()?
        .is_some())
}

fn provider_state_warnings(
    conn: &Connection,
    config: Option<&str>,
    model: Option<&str>,
    line: u64,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let provider_id = config
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|value| {
            value
                .get("provider_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if let Some(provider_id) = provider_id.as_deref() {
        let enabled: Option<i64> = conn
            .query_row(
                "SELECT enabled FROM providers WHERE id = ?1",
                [provider_id],
                |row| row.get(0),
            )
            .optional()?;
        if enabled != Some(1) {
            warnings.push(
                "provider_unavailable",
                line,
                format!("供应商 {provider_id} 缺失或停用；保留原值，会话只读"),
            );
        }
    }
    if let Some(model) = model {
        let found: Option<i64> = match provider_id.as_deref() {
            Some(provider_id) => conn
                .query_row(
                    "SELECT enabled FROM provider_models WHERE provider_id = ?1 AND model_id = ?2",
                    params![provider_id, model],
                    |row| row.get(0),
                )
                .optional()?,
            None => conn
                .query_row(
                    "SELECT enabled FROM provider_models WHERE model_id = ?1 LIMIT 1",
                    [model],
                    |row| row.get(0),
                )
                .optional()?,
        };
        if found != Some(1) {
            warnings.push(
                "model_unavailable",
                line,
                format!("模型 {model} 缺失或停用/不属于供应商；保留原值，会话只读"),
            );
        }
    }
    Ok(())
}

fn next_seq(conn: &Connection, kind: &str, session_id: &str) -> Result<i64, ImportFailure> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT next_seq FROM import_seq WHERE kind = ?1 AND session_id = ?2",
            params![kind, session_id],
            |row| row.get(0),
        )
        .optional()?;
    let next = match current {
        Some(value) => value,
        None => {
            let table = if kind == "message" { "messages" } else { "events" };
            let sql = format!("SELECT COALESCE(MAX(seq), 0) FROM {table} WHERE session_id = ?1");
            let max: i64 = conn.query_row(&sql, [session_id], |row| row.get(0))?;
            max.saturating_add(1)
        }
    };
    conn.execute(
        "INSERT INTO import_seq (kind, session_id, next_seq) VALUES (?1, ?2, ?3) \
         ON CONFLICT(kind, session_id) DO UPDATE SET next_seq = excluded.next_seq",
        params![kind, session_id, next.saturating_add(1)],
    )?;
    Ok(next)
}

fn truncate_title(text: &str) -> String {
    text.chars().take(256).collect()
}

fn process_session(
    conn: &Connection,
    value: &Value,
    mode: ImportMode,
    line: u64,
    at_ms: i64,
    summary: &mut ImportSummary,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let old_id = required_str(value, "id", "session", line)?;
    if mode == ImportMode::Preserve && id_exists(conn, "session", old_id)? {
        return Err(ImportFailure::conflict(
            format!("preserve 模式会话 id 冲突：{old_id}"),
            old_id,
            "session",
        ));
    }
    let new_id = if mode == ImportMode::Preserve {
        old_id.to_owned()
    } else {
        loop {
            let candidate = new_ulid();
            if !id_exists(conn, "session", &candidate)?
                && mapped_id(conn, "session", &candidate)?.is_none()
            {
                break candidate;
            }
        }
    };
    insert_mapping(conn, "session", old_id, &new_id, Some(old_id), Some(&new_id))?;

    let runtime_id = required_str(value, "runtime_id", "session", line)?;
    ensure_runtime(conn, runtime_id, at_ms)?;

    let workspace_id = optional_str(value, "workspace_id", "session", line)?;
    let workspace_id = match workspace_id {
        Some(id) if workspace_exists(conn, id)? => Some(id.to_owned()),
        Some(id) => {
            warnings.push(
                "workspace_missing",
                line,
                format!("工作区 {id} 不存在；会话按无工作区导入（只读风险）"),
            );
            None
        }
        None => None,
    };
    let parent_old = optional_str(value, "parent_session_id", "session", line)?;
    if let Some(parent_old) = parent_old {
        conn.execute(
            "INSERT INTO import_session_parents (session_new, parent_old) VALUES (?1, ?2)",
            params![new_id, parent_old],
        )?;
    }

    let title = optional_str(value, "title", "session", line)?
        .map(str::to_owned)
        .unwrap_or_else(|| {
            warnings.push(
                "session_title_defaulted",
                line,
                "会话标题缺失；使用 Imported session <id 前 8 位>",
            );
            format!("Imported session {}", &new_id[..new_id.len().min(8)])
        });
    let title = if title.chars().count() > 256 {
        warnings.push("session_title_truncated", line, "标题超过 256 字符；已截断");
        truncate_title(&title)
    } else {
        title
    };
    let status = required_str(value, "status", "session", line)?;
    let model = optional_str(value, "model", "session", line)?;
    let system_prompt = optional_str(value, "system_prompt", "session", line)?;
    let config = optional_object_text(value, "config", "session", line)?;
    let token_usage = optional_object_text(value, "token_usage", "session", line)?;
    let thinking_depth = optional_i64(value, "thinking_depth", "session", line)?.unwrap_or(2);
    let created_at = required_i64(value, "created_at", "session", line)?;
    let updated_at = required_i64(value, "updated_at", "session", line)?;
    let closed_at = optional_i64(value, "closed_at", "session", line)?;
    provider_state_warnings(conn, config.as_deref(), model, line, warnings)?;
    conn.execute(
        "INSERT INTO sessions \
         (id, runtime_id, workspace_id, parent_session_id, title, status, model, system_prompt, \
          config, token_usage, created_at, updated_at, closed_at, thinking_depth, permission_mode, \
          preset_id) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'workspace_write', NULL)",
        params![
            new_id,
            runtime_id,
            workspace_id,
            title,
            status,
            model,
            system_prompt,
            config.unwrap_or_else(|| "{}".to_owned()),
            token_usage.unwrap_or_else(|| "{}".to_owned()),
            created_at,
            updated_at,
            closed_at,
            thinking_depth,
        ],
    )?;
    summary.sessions = summary.sessions.saturating_add(1);
    Ok(())
}

fn process_run(
    conn: &Connection,
    value: &Value,
    mode: ImportMode,
    line: u64,
    summary: &mut ImportSummary,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let old_id = required_str(value, "id", "run", line)?;
    let session_old = required_str(value, "session_id", "run", line)?;
    let Some(session_new) = mapped_id(conn, "session", session_old)? else {
        summary.skipped = summary.skipped.saturating_add(1);
        warnings.push(
            "run_session_missing",
            line,
            format!("run {old_id} 的会话 {session_old} 不在导入文件内；已跳过"),
        );
        return Ok(());
    };
    if mode == ImportMode::Preserve && id_exists(conn, "run", old_id)? {
        return Err(ImportFailure::conflict(
            format!("preserve 模式 run id 冲突：{old_id}"),
            old_id,
            "run",
        ));
    }
    let new_id = if mode == ImportMode::Preserve {
        old_id.to_owned()
    } else {
        loop {
            let candidate = new_ulid();
            if !id_exists(conn, "run", &candidate)? {
                break candidate;
            }
        }
    };
    insert_mapping(conn, "run", old_id, &new_id, Some(session_old), Some(&session_new))?;
    let status = required_str(value, "status", "run", line)?;
    let input_old = optional_str(value, "input_message_id", "run", line)?;
    if let Some(input_old) = input_old {
        conn.execute(
            "INSERT INTO import_run_inputs (run_new, message_old) VALUES (?1, ?2)",
            params![new_id, input_old],
        )?;
    }
    let thinking_depth = optional_i64(value, "thinking_depth", "run", line)?;
    let error = optional_str(value, "error", "run", line)?;
    let started_at = required_i64(value, "started_at", "run", line)?;
    let finished_at = optional_i64(value, "finished_at", "run", line)?;
    conn.execute(
        "INSERT INTO runs \
         (id, session_id, status, input_message_id, error, started_at, finished_at, thinking_depth) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
        params![
            new_id,
            session_new,
            status,
            error,
            started_at,
            finished_at,
            thinking_depth
        ],
    )?;
    summary.runs = summary.runs.saturating_add(1);
    Ok(())
}

fn process_audit(
    conn: &Connection,
    value: &Value,
    mode: ImportMode,
    line: u64,
    summary: &mut ImportSummary,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let old_id = required_str(value, "id", "audit", line)?;
    if mode == ImportMode::Preserve && id_exists(conn, "audit", old_id)? {
        return Err(ImportFailure::conflict(
            format!("preserve 模式 audit id 冲突：{old_id}"),
            old_id,
            "audit",
        ));
    }
    let new_id = if mode == ImportMode::Preserve {
        old_id.to_owned()
    } else {
        loop {
            let candidate = new_ulid();
            if !id_exists(conn, "audit", &candidate)? {
                break candidate;
            }
        }
    };
    let session_old = optional_str(value, "session_id", "audit", line)?;
    let session_new = match session_old {
        Some(old) => match mapped_id(conn, "session", old)? {
            Some(new) => Some(new),
            None => {
                warnings.push(
                    "audit_session_missing",
                    line,
                    format!("审计 session_id {old} 不在导入文件内；已置 NULL"),
                );
                None
            }
        },
        None => None,
    };
    let runtime_id = optional_str(value, "runtime_id", "audit", line)?;
    let actor = required_str(value, "actor", "audit", line)?;
    let action = required_str(value, "action", "audit", line)?;
    let resource = optional_str(value, "resource", "audit", line)?;
    let detail = value.get("detail").cloned().unwrap_or(Value::Null);
    let detail = match detail {
        Value::Null => None,
        Value::String(text) => Some(text),
        other => Some(serde_json::to_string(&other)?),
    };
    let result = optional_str(value, "result", "audit", line)?;
    let ts = required_i64(value, "ts", "audit", line)?;
    conn.execute(
        "INSERT INTO audit_log \
         (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new_id,
            session_new,
            runtime_id,
            actor,
            action,
            resource,
            detail,
            result,
            ts
        ],
    )?;
    summary.audits = summary.audits.saturating_add(1);
    Ok(())
}

fn process_message(
    conn: &Connection,
    value: &Value,
    mode: ImportMode,
    line: u64,
    summary: &mut ImportSummary,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let old_id = required_str(value, "id", "message", line)?;
    let session_old = required_str(value, "session_id", "message", line)?;
    let Some(session_new) = mapped_id(conn, "session", session_old)? else {
        summary.skipped = summary.skipped.saturating_add(1);
        warnings.push(
            "message_session_missing",
            line,
            format!("消息 {old_id} 的会话 {session_old} 不在导入文件内；已跳过"),
        );
        return Ok(());
    };
    if mode == ImportMode::Preserve && id_exists(conn, "message", old_id)? {
        return Err(ImportFailure::conflict(
            format!("preserve 模式 message id 冲突：{old_id}"),
            old_id,
            "message",
        ));
    }
    let client_msg_id = optional_str(value, "client_msg_id", "message", line)?;
    if let Some(client_msg_id) = client_msg_id {
        let duplicate: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM messages WHERE session_id = ?1 AND client_msg_id = ?2 LIMIT 1",
                params![session_new, client_msg_id],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            summary.duplicates = summary.duplicates.saturating_add(1);
            warnings.push(
                "message_duplicate",
                line,
                format!("client_msg_id {client_msg_id} 已存在；跳过该消息"),
            );
            return Ok(());
        }
    }
    let new_id = if mode == ImportMode::Preserve {
        old_id.to_owned()
    } else {
        loop {
            let candidate = new_ulid();
            if !id_exists(conn, "message", &candidate)? {
                break candidate;
            }
        }
    };
    insert_mapping(
        conn,
        "message",
        old_id,
        &new_id,
        Some(session_old),
        Some(&session_new),
    )?;
    let role = required_str(value, "role", "message", line)?;
    let content = value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ts = required_i64(value, "ts", "message", line)?;
    let seq = next_seq(conn, "message", &session_new)?;
    conn.execute(
        "INSERT INTO messages (id, session_id, role, content, seq, created_at, client_msg_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![new_id, session_new, role, content, seq, ts, client_msg_id],
    )?;
    summary.messages = summary.messages.saturating_add(1);
    Ok(())
}

fn process_event(
    conn: &Connection,
    value: &Value,
    mode: ImportMode,
    line: u64,
    summary: &mut ImportSummary,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    let old_id = required_str(value, "id", "event", line)?;
    let session_old = required_str(value, "session_id", "event", line)?;
    let session_new = match mapped_id(conn, "session", session_old)? {
        Some(new) => new,
        None => {
            if mode == ImportMode::Remap {
                warnings.push(
                    "event_session_not_mapped",
                    line,
                    format!("事件 {old_id} 的会话 {session_old} 不在导入文件内；保留原会话 id"),
                );
            }
            session_old.to_owned()
        }
    };
    if mode == ImportMode::Preserve && id_exists(conn, "event", old_id)? {
        return Err(ImportFailure::conflict(
            format!("preserve 模式 event id 冲突：{old_id}"),
            old_id,
            "event",
        ));
    }
    let new_id = if mode == ImportMode::Preserve {
        old_id.to_owned()
    } else {
        loop {
            let candidate = new_ulid();
            if !id_exists(conn, "event", &candidate)? {
                break candidate;
            }
        }
    };
    insert_mapping(
        conn,
        "event",
        old_id,
        &new_id,
        Some(session_old),
        Some(&session_new),
    )?;
    let run_old = optional_str(value, "run_id", "event", line)?;
    if let Some(run_old) = run_old {
        conn.execute(
            "INSERT INTO import_event_runs (event_new, run_old) VALUES (?1, ?2)",
            params![new_id, run_old],
        )?;
    }
    let runtime_id = required_str(value, "runtime_id", "event", line)?;
    let event_type = required_str(value, "type", "event", line)?;
    let payload = serde_json::to_string(
        value
            .get("payload")
            .ok_or_else(|| invalid_detail("event", line, "payload", "缺少 payload"))?,
    )?;
    let ts = required_i64(value, "ts", "event", line)?;
    let version = required_i64(value, "v", "event", line)?;
    let seq = next_seq(conn, "event", &session_new)?;
    conn.execute(
        "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            new_id,
            session_new,
            runtime_id,
            seq,
            event_type,
            payload,
            ts,
            version
        ],
    )?;
    summary.events = summary.events.saturating_add(1);
    Ok(())
}

fn create_temp_tables(conn: &Connection) -> Result<(), ImportFailure> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_id_map (\
           kind TEXT NOT NULL, old_id TEXT NOT NULL, new_id TEXT NOT NULL, \
           session_old TEXT, session_new TEXT, PRIMARY KEY (kind, old_id));\
         CREATE TABLE IF NOT EXISTS import_seq (\
           kind TEXT NOT NULL, session_id TEXT NOT NULL, next_seq INTEGER NOT NULL, \
           PRIMARY KEY (kind, session_id));\
         CREATE TABLE IF NOT EXISTS import_session_parents (\
           session_new TEXT PRIMARY KEY, parent_old TEXT NOT NULL);\
         CREATE TABLE IF NOT EXISTS import_run_inputs (\
           run_new TEXT PRIMARY KEY, message_old TEXT NOT NULL);\
         CREATE TABLE IF NOT EXISTS import_event_runs (\
           event_new TEXT PRIMARY KEY, run_old TEXT NOT NULL);",
    )?;
    Ok(())
}

fn begin(conn: &Connection) -> Result<(), ImportFailure> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    Ok(())
}

fn commit(conn: &Connection) -> Result<(), ImportFailure> {
    conn.execute_batch("COMMIT;")?;
    Ok(())
}

fn rollback(conn: &Connection) {
    let _ = conn.execute_batch("ROLLBACK;");
}

/// 生成 staging 快照（`VACUUM INTO`；不修改当前库）。
pub fn create_staging_snapshot(
    live_db: Option<&Path>,
    work_db: &Path,
) -> Result<(), ImportFailure> {
    remove_db_files(work_db)?;
    if let Some(parent) = work_db.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(live_db) = live_db.filter(|path| path.exists()) {
        let live = Connection::open_with_flags(
            live_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        pragma::apply_read_only(&live)?;
        let report = quick_check(&live);
        if !report.ok {
            return Err(ImportFailure::internal(format!(
                "当前数据库 quick_check 失败：{}",
                report.summary()
            )));
        }
        let work_text = work_db
            .to_str()
            .ok_or_else(|| ImportFailure::internal("staging 路径不是合法 UTF-8"))?;
        live.execute("VACUUM INTO ?1", params![work_text])?;
    }
    let mut connection = Connection::open(work_db)?;
    pragma::apply(&connection)?;
    migration::migrate(&mut connection).map_err(ImportFailure::from)?;
    connection.pragma_update(None, "journal_mode", "MEMORY")?;
    connection.pragma_update(None, "synchronous", "OFF")?;
    Ok(())
}

/// 将导入源应用到 staging（两遍扫描；每批 ≤1000 行；可取消）。
pub fn apply_source_to_staging(
    source_path: &Path,
    work_db: &Path,
    expected_sources: &[&str],
    mode: ImportMode,
    at_ms: i64,
    progress: &mut dyn FnMut(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportApplyReport, ImportFailure> {
    let started = Instant::now();
    let connection = Connection::open(work_db)?;
    pragma::apply(&connection)?;
    connection.pragma_update(None, "journal_mode", "MEMORY")?;
    connection.pragma_update(None, "synchronous", "OFF")?;
    create_temp_tables(&connection)?;

    let mut summary = ImportSummary::default();
    let mut warnings = WarningSink::new();

    // Pass A：sessions / runs / audit（先建立 ID 映射；runs 在 events 之前）。
    begin(&connection)?;
    let mut batch_rows = 0usize;
    let pass_a = scan_source(
        source_path,
        expected_sources,
        |kind, value, line| {
            if cancelled() {
                return Err(ImportFailure::cancelled());
            }
            let disposition = validate_row(kind, value, line, &mut warnings)?;
            if disposition == RowDisposition::Skip {
                return Ok(());
            }
            match kind {
                "session" => process_session(
                    &connection,
                    value,
                    mode,
                    line,
                    at_ms,
                    &mut summary,
                    &mut warnings,
                )?,
                "run" => process_run(&connection, value, mode, line, &mut summary, &mut warnings)?,
                "audit" => {
                    process_audit(&connection, value, mode, line, &mut summary, &mut warnings)?
                }
                _ => {}
            }
            batch_rows = batch_rows.saturating_add(1);
            if batch_rows >= IMPORT_BATCH_ROWS {
                commit(&connection)?;
                begin(&connection)?;
                batch_rows = 0;
            }
            Ok(())
        },
        progress,
        cancelled,
    );
    let pass_a = match pass_a {
        Ok(scan) => {
            commit(&connection)?;
            scan
        }
        Err(error) => {
            rollback(&connection);
            return Err(error);
        }
    };

    // Pass B：messages / events（seq 重排 + client_msg_id 去重）。
    begin(&connection)?;
    let mut batch_rows = 0usize;
    let pass_b = scan_source(
        source_path,
        expected_sources,
        |kind, value, line| {
            if cancelled() {
                return Err(ImportFailure::cancelled());
            }
            let disposition = validate_row(kind, value, line, &mut warnings)?;
            if disposition == RowDisposition::Skip {
                summary.skipped = summary.skipped.saturating_add(1);
                return Ok(());
            }
            match kind {
                "message" => {
                    process_message(&connection, value, mode, line, &mut summary, &mut warnings)?
                }
                "event" => {
                    process_event(&connection, value, mode, line, &mut summary, &mut warnings)?
                }
                _ => {}
            }
            batch_rows = batch_rows.saturating_add(1);
            if batch_rows >= IMPORT_BATCH_ROWS {
                commit(&connection)?;
                begin(&connection)?;
                batch_rows = 0;
            }
            Ok(())
        },
        progress,
        cancelled,
    );
    let pass_b = match pass_b {
        Ok(scan) => {
            commit(&connection)?;
            scan
        }
        Err(error) => {
            rollback(&connection);
            return Err(error);
        }
    };

    if pass_a.rows != pass_b.rows || pass_a.sha256 != pass_b.sha256 {
        return Err(ImportFailure::invalid_format(
            "导入源在读取过程中发生变化（两遍哈希不一致）；请重试",
        ));
    }

    // 延迟引用映射：parent_session_id / run.input_message_id / event.run_id。
    resolve_deferred(&connection, mode, &mut warnings)?;

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(ImportApplyReport {
        summary,
        rows: pass_b.rows,
        bytes: pass_b.bytes,
        sha256: pass_b.sha256,
        duration_ms,
        warnings: warnings.warnings,
        warnings_truncated: warnings.truncated,
    })
}

/// 解析延迟引用（在全部 ID 映射建立后执行）。
fn resolve_deferred(
    conn: &Connection,
    mode: ImportMode,
    warnings: &mut WarningSink,
) -> Result<(), ImportFailure> {
    begin(conn)?;
    let mut parent_rows = Vec::new();
    {
        let mut statement = conn.prepare("SELECT session_new, parent_old FROM import_session_parents")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            parent_rows.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
        }
    }
    for (session_new, parent_old) in parent_rows {
        let mapped = mapped_id(conn, "session", &parent_old)?;
        let target = match mapped {
            Some(new) => Some(new),
            None if mode == ImportMode::Preserve && id_exists(conn, "session", &parent_old)? => {
                Some(parent_old.clone())
            }
            None => {
                warnings.push(
                    "session_parent_missing",
                    0,
                    format!("父会话 {parent_old} 不在导入文件内；已置 NULL"),
                );
                None
            }
        };
        if let Some(target) = target {
            conn.execute(
                "UPDATE sessions SET parent_session_id = ?1 WHERE id = ?2",
                params![target, session_new],
            )?;
        }
    }

    let mut run_inputs = Vec::new();
    {
        let mut statement = conn.prepare("SELECT run_new, message_old FROM import_run_inputs")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            run_inputs.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
        }
    }
    for (run_new, message_old) in run_inputs {
        match mapped_id(conn, "message", &message_old)? {
            Some(message_new) => {
                conn.execute(
                    "UPDATE runs SET input_message_id = ?1 WHERE id = ?2",
                    params![message_new, run_new],
                )?;
            }
            None => warnings.push(
                "run_input_missing",
                0,
                format!("run 输入消息 {message_old} 不在导入文件内；已置 NULL"),
            ),
        }
    }

    let mut event_runs = Vec::new();
    {
        let mut statement = conn.prepare("SELECT event_new, run_old FROM import_event_runs")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            event_runs.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
        }
    }
    for (event_new, run_old) in event_runs {
        match mapped_id(conn, "run", &run_old)? {
            Some(run_new) => {
                conn.execute(
                    "UPDATE events SET run_id = ?1 WHERE id = ?2",
                    params![run_new, event_new],
                )?;
            }
            None => warnings.push(
                "event_run_missing",
                0,
                format!("事件 run_id {run_old} 不在导入文件内；已置 NULL"),
            ),
        }
    }
    commit(conn)?;
    Ok(())
}

/// 写入 staging 完成标记（已登记审计动作 `import.completed`；不新增设置键）。
///
/// 标记行进入 staging 后随原子切换落库；kill 后据此判定切换已完成（幂等）。
pub fn mark_staging(
    work_db: &Path,
    job_id: &str,
    at_ms: i64,
    detail: Value,
) -> Result<(), ImportFailure> {
    let mut detail = detail;
    if let Some(object) = detail.as_object_mut() {
        object.insert("marker".to_owned(), Value::Bool(true));
    }
    write_audit(
        work_db,
        AUDIT_ACTION_IMPORT_COMPLETED,
        "success",
        job_id,
        detail,
        at_ms,
    )?;
    let connection = Connection::open(work_db)?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.execute_batch("VACUUM;")?;
    Ok(())
}

/// 当前库是否已包含该作业的完成标记（kill 后幂等判定；审计行即标记）。
pub fn has_marker(live_db: &Path, job_id: &str) -> bool {
    if !live_db.exists() {
        return false;
    }
    let Ok(connection) = Connection::open_with_flags(
        live_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    connection
        .query_row(
            "SELECT 1 FROM audit_log \
             WHERE action = ?1 AND resource = ?2 LIMIT 1",
            params![AUDIT_ACTION_IMPORT_COMPLETED, job_id],
            |_row| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// staging 验收：`quick_check` + schema 版本 + 外键检查。
pub fn verify_staging(work_db: &Path) -> Result<StagingVerification, ImportFailure> {
    let connection = Connection::open_with_flags(
        work_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let report = quick_check(&connection);
    if !report.ok {
        return Err(ImportFailure::internal(format!(
            "staging quick_check 失败：{}",
            report.summary()
        )));
    }
    let schema_version: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if schema_version > program_schema_version() {
        return Err(ImportFailure::invalid_format(format!(
            "staging schema v{schema_version} 高于程序 v{}",
            program_schema_version()
        )));
    }
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let violations = statement.query_map([], |row| row.get::<_, String>(0))?.count();
    Ok(StagingVerification {
        bytes: file_len(work_db),
        schema_version,
        foreign_key_violations: violations,
    })
}

fn write_audit(
    db_path: &Path,
    action: &str,
    result: &str,
    job_id: &str,
    detail: Value,
    at_ms: i64,
) -> Result<(), ImportFailure> {
    let connection = Connection::open(db_path)?;
    let existing: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM audit_log WHERE action = ?1 AND resource = ?2 LIMIT 1",
            params![action, job_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO audit_log \
         (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
         VALUES (?1, NULL, NULL, 'user', ?2, ?3, ?4, ?5, ?6)",
        params![
            new_ulid(),
            action,
            job_id,
            serde_json::to_string(&detail)?,
            result,
            at_ms
        ],
    )?;
    Ok(())
}

fn fail_job(
    data_dir: &Path,
    job: &mut ImportJob,
    failure: &ImportFailure,
    at_ms: i64,
) -> Result<(), ImportFailure> {
    job.status = ImportJobStatus::Failed;
    job.error = Some(ImportFailureInfo {
        code: failure.business_code().to_owned(),
        message: failure.message.clone(),
        details: failure.details.clone(),
    });
    job.updated_at = at_ms;
    write_job(data_dir, job)?;
    Ok(())
}

/// 启动序列：应用待重启导入（原子切换；失败/中断不改变既有数据）。
///
/// 返回 `Ok(None)` = 无待处理导入；`Err` = 已有作业已被标记 `failed`。
pub fn boot_apply_pending_import<F>(
    data_dir: &Path,
    at_ms: i64,
    available_bytes: F,
) -> Result<Option<ImportBootReport>, ImportFailure>
where
    F: Fn(&Path) -> Result<u64, String>,
{
    let _ = cleanup_imports(data_dir, at_ms);
    let _ = reconcile_interrupted_imports(data_dir, at_ms);
    let Some(pending) = read_pending(data_dir)? else {
        return Ok(None);
    };
    let job_id = pending.job_id.clone();
    let Some(mut job) = read_job(data_dir, &job_id)? else {
        let _ = remove_pending(data_dir);
        return Err(ImportFailure::job_not_found(&job_id));
    };
    let started = Instant::now();
    let db_path = data_dir.join("aether.db");

    // kill 后幂等：切换已完成但作业状态未落盘。
    if has_marker(&db_path, &job_id) {
        job.status = ImportJobStatus::Completed;
        job.updated_at = at_ms;
        write_job(data_dir, &job)?;
        let _ = remove_pending(data_dir);
        let _ = cleanup_staging(data_dir, &job_id);
        let _ = write_audit(
            &db_path,
            AUDIT_ACTION_IMPORT_COMPLETED,
            "success",
            &job_id,
            json!({
                "source": job.source_path,
                "mode": job.mode.as_str(),
                "summary": job.summary,
                "already_applied": true,
            }),
            at_ms,
        );
        return Ok(Some(ImportBootReport {
            job_id,
            kind: job.kind,
            mode: job.mode,
            summary: job.summary.unwrap_or_default(),
            already_applied: true,
            duration_ms: 0,
        }));
    }

    if job.status != ImportJobStatus::AwaitingRestart {
        let failure = if job.status == ImportJobStatus::Applying {
            ImportFailure::internal("上次导入在切换阶段被中断（kill -9）；已清理 staging，不自动续跑")
        } else {
            ImportFailure::internal(format!(
                "导入作业状态 {} 不允许启动切换",
                job.status.as_str()
            ))
        };
        let _ = fail_job(data_dir, &mut job, &failure, at_ms);
        let _ = remove_pending(data_dir);
        let _ = cleanup_staging(data_dir, &job_id);
        let _ = write_audit(
            &db_path,
            AUDIT_ACTION_IMPORT_FAILED,
            "failed",
            &job_id,
            json!({"error": failure.message}),
            at_ms,
        );
        return Err(failure);
    }

    // 空间检查：预期增量×1.2 + 当前 db+wal×1.2（ADR-034 D-034-2）。
    let source_path = PathBuf::from(&job.source_path);
    let source_bytes = file_len(&source_path);
    let required = required_space_bytes(source_bytes, 0)
        .saturating_add(required_space_bytes(
            file_len(&db_path),
            file_len(&sidecar(&db_path, "-wal")),
        ));
    let available = available_bytes(data_dir)
        .map_err(|message| ImportFailure::internal(format!("空间探测失败：{message}")))?;
    if available < required {
        let failure = ImportFailure::too_large(format!(
            "数据目录可用空间不足（import_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节"
        ));
        let _ = fail_job(data_dir, &mut job, &failure, at_ms);
        let _ = remove_pending(data_dir);
        let _ = write_audit(
            &db_path,
            AUDIT_ACTION_IMPORT_FAILED,
            "failed",
            &job_id,
            json!({"error": failure.message}),
            at_ms,
        );
        return Err(failure);
    }

    job.status = ImportJobStatus::Applying;
    job.updated_at = at_ms;
    write_job(data_dir, &job)?;

    let expected_sources: &[&str] = if job.kind == "session" {
        &SESSION_IMPORT_SOURCES
    } else {
        &ARCHIVE_IMPORT_SOURCES
    };
    let work_db = work_db_path(data_dir, &job_id);
    let result = (|| -> Result<ImportApplyReport, ImportFailure> {
        create_staging_snapshot(Some(&db_path), &work_db)?;
        let report = apply_source_to_staging(
            &source_path,
            &work_db,
            expected_sources,
            job.mode,
            at_ms,
            &mut |_bytes, _rows| {},
            &|| false,
        )?;
        let verification = verify_staging(&work_db)?;
        if verification.foreign_key_violations > 0 {
            return Err(ImportFailure::internal(format!(
                "staging 外键校验失败：{} 条悬挂引用",
                verification.foreign_key_violations
            )));
        }
        mark_staging(
            &work_db,
            &job_id,
            at_ms,
            json!({
                "source": job.source_path,
                "mode": job.mode.as_str(),
                "summary": report.summary,
                "warnings": report.warnings.len(),
                "duration_ms": report.duration_ms,
            }),
        )?;
        let request = RestoreRequest {
            db_path: db_path.clone(),
            candidate_path: work_db.clone(),
            program_max_version: program_schema_version(),
            now_ms: at_ms,
            fault: RestoreFault::None,
            scene_infix: PRE_IMPORT_INFIX,
        };
        execute_restore(&request, || Ok(())).map_err(|error| {
            ImportFailure::internal(format!("staging 原子切换失败（已回滚现场）：{error}"))
        })?;
        let _ = finalize_restore(&db_path);
        Ok(report)
    })();

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(report) => {
            job.status = ImportJobStatus::Completed;
            job.summary = Some(report.summary);
            job.progress = ImportProgress {
                rows: report.rows,
                bytes: report.bytes,
                total_bytes: Some(report.bytes),
            };
            job.warnings = report.warnings;
            job.warnings_truncated = report.warnings_truncated;
            job.error = None;
            job.updated_at = at_ms;
            write_job(data_dir, &job)?;
            let _ = remove_pending(data_dir);
            let _ = cleanup_staging(data_dir, &job_id);
            let _ = write_audit(
                &db_path,
                AUDIT_ACTION_IMPORT_COMPLETED,
                "success",
                &job_id,
                json!({
                    "source": job.source_path,
                    "mode": job.mode.as_str(),
                    "summary": job.summary,
                    "warnings": job.warnings.len(),
                    "duration_ms": duration_ms,
                }),
                at_ms,
            );
            Ok(Some(ImportBootReport {
                job_id,
                kind: job.kind,
                mode: job.mode,
                summary: report.summary,
                already_applied: false,
                duration_ms,
            }))
        }
        Err(failure) => {
            let _ = fail_job(data_dir, &mut job, &failure, at_ms);
            let _ = remove_pending(data_dir);
            let _ = cleanup_staging(data_dir, &job_id);
            let _ = write_audit(
                &db_path,
                AUDIT_ACTION_IMPORT_FAILED,
                "failed",
                &job_id,
                json!({"error": failure.message, "duration_ms": duration_ms}),
                at_ms,
            );
            Err(failure)
        }
    }
}

/// 读取作业并返回 `import_status` 载荷（调用方决定是否裁剪 warnings）。
pub fn job_status_value(job: &ImportJob, include_warnings: bool) -> Value {
    let mut value = serde_json::to_value(job).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        if !include_warnings {
            object.remove("warnings");
        }
        object.insert("status".to_owned(), Value::String(job.status.as_str().to_owned()));
        object.insert("mode".to_owned(), Value::String(job.mode.as_str().to_owned()));
    }
    value
}

/// 当前时间（毫秒；供 app 层构造作业）。
pub fn current_time_ms() -> i64 {
    now_ms()
}

/// 默认可用空间探测不使用 Store 依赖（由 app 注入）；保留时长常量供调用方。
pub fn stale_window() -> Duration {
    Duration::from_secs((IMPORT_STALE_HOURS * 3_600) as u64)
}
