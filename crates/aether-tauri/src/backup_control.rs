//! 备份/恢复命令后端（M3-04；设计 D13、ADR-003 决策 19、ADR-004 命令面）。
//!
//! 覆盖 D7 命令面中的备份族（薄适配，业务语义全部在 `aether-store` 的恢复引擎）：
//! - `backup_create`：手动备份（`VACUUM INTO` 经**只读连接**，D4 降级期入口可用）；
//!   目标目录缺省为数据目录 `backups/`，或用户选择的外部目录（D13）；写出前校验
//!   目标可写 + 可用空间 ≥ 当前 `db+wal` ×1.2（ADR-003 决策 19，不足拒绝并提示）；
//!   登记 `backups` 台账并执行保留策略（最近 [`aether_store::BACKUP_RETENTION`] 份）；
//! - `backup_list`：内部备份清单（`backups` 表，最新在前）+ 容量状态（2GB/5GB，D13）；
//! - `backup_restore`：候选校验（内部 id / 外部 `.db`；只读打开 + `quick_check` +
//!   迁移版本检查，高版本拒绝并提示升级）→ 登记恢复现场日志（D13 七步的 1–2 步）
//!   → 命令层请求应用重启，第 3–6 步在下次启动序列（无写者窗口）执行；审计在
//!   请求与启动结果两处写入（D13 第 7 步）。
//!
//! 桥接口径与 `session_backend` 一致：async 存储操作 spawn 到核心运行时 +
//! `std::sync::mpsc` 同步等待（不阻塞运行时线程）。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_store::backup::{
    program_schema_version, prune_plan, request_restore, required_space_bytes, validate_candidate,
    BackupRecord, BACKUP_RETENTION,
};
use aether_store::{
    create_backup_file, recover_or_apply_pending_restore, AuditLogRecord, ReadPool,
    RestoreStartupOutcome, StoreCommand, StoreError, WriteQueue,
};
use serde_json::{json, Value};

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    AppRestartRequest, BackupCreateRequest, BackupRestoreRequest, BackupSource,
    ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, RunRetryRequest, RuntimeEnableRequest, RuntimeRetryRequest,
    SessionCreateRequest, SessionIdRequest, SessionListRequest, SessionSendRequest,
    SettingsGetRequest, SettingsSetRequest, WorkspaceSetRequest,
};
use crate::ipc::error::IpcError;

/// 备份命令硬超时（大库 `VACUUM INTO` 预算；与 D6 方法超时表独立，手动动作给足窗口）。
pub const BACKUP_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// 容量巡检警告阈值（D13：2GB）。
pub const CAPACITY_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 容量巡检强烈提示阈值（D13：5GB）。
pub const CAPACITY_CRITICAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// 容量阈值覆盖环境变量（M3-05 DoD2「阈值参数化模拟」；仅调参，不改变语义）。
pub const CAPACITY_WARN_ENV: &str = "AETHER_CAPACITY_WARN_BYTES";
/// 容量强提示阈值覆盖环境变量（M3-05 DoD2）。
pub const CAPACITY_CRITICAL_ENV: &str = "AETHER_CAPACITY_CRITICAL_BYTES";
/// 备份台账 `kind`：应用 `backups` 目录。
pub const BACKUP_KIND_INTERNAL: &str = "internal";
/// 备份台账 `kind`：用户选择的外部目录（M3-04 登记）。
pub const BACKUP_KIND_EXTERNAL: &str = "external";
/// 审计动作（D13 第 7 步）。
pub const AUDIT_ACTION_RESTORE: &str = "backup.restore";

/// 容量阈值配置（D13：2GB 警告 / 5GB 强烈提示；M3-05 起参数化注入）。
///
/// 语义不变，仅允许测试/演练通过构造注入或环境变量模拟阈值（常量级调参，
/// 记录于任务证据；不改变 `ok|warn|critical` 三档口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityConfig {
    pub warn_bytes: u64,
    pub critical_bytes: u64,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            warn_bytes: CAPACITY_WARN_BYTES,
            critical_bytes: CAPACITY_CRITICAL_BYTES,
        }
    }
}

impl CapacityConfig {
    /// 以显式阈值构造（测试/演练注入）。
    pub fn with_limits(warn_bytes: u64, critical_bytes: u64) -> Self {
        Self {
            warn_bytes,
            critical_bytes,
        }
    }

    /// 从环境变量读取覆盖（未设置/非法 → 默认值；仅调参）。
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(value) = parse_env_bytes(CAPACITY_WARN_ENV) {
            config.warn_bytes = value;
        }
        if let Some(value) = parse_env_bytes(CAPACITY_CRITICAL_ENV) {
            config.critical_bytes = value;
        }
        config
    }

    /// 容量等级：`ok` / `warn` / `critical`（与 M3-04 `backup_list` 同一口径）。
    pub fn level(&self, total_bytes: u64) -> &'static str {
        if total_bytes >= self.critical_bytes {
            "critical"
        } else if total_bytes >= self.warn_bytes {
            "warn"
        } else {
            "ok"
        }
    }

    /// 容量投影（`backup_list.capacity` 与诊断包共用形状）。
    pub fn to_json(&self, db_bytes: u64, wal_bytes: u64) -> Value {
        let total_bytes = db_bytes.saturating_add(wal_bytes);
        json!({
            "db_bytes": db_bytes,
            "wal_bytes": wal_bytes,
            "total_bytes": total_bytes,
            "warn_bytes": self.warn_bytes,
            "critical_bytes": self.critical_bytes,
            "level": self.level(total_bytes),
        })
    }
}

fn parse_env_bytes(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    raw.trim().parse::<u64>().ok()
}

/// 空间护栏探针（ADR-003 决策 19；生产为 [`NativeSpaceProbe`]，测试注入固定值）。
pub trait SpaceProbe: Send + Sync + 'static {
    /// 目录可写校验。
    fn ensure_writable(&self, dir: &Path) -> Result<(), String>;
    /// 目录可用空间（字节）。
    fn available_bytes(&self, dir: &Path) -> Result<u64, String>;
}

/// 原生探针：真实磁盘（`crate::disk`；迁移探针同源）。
pub struct NativeSpaceProbe;

impl SpaceProbe for NativeSpaceProbe {
    fn ensure_writable(&self, dir: &Path) -> Result<(), String> {
        crate::disk::ensure_writable(dir)
    }

    fn available_bytes(&self, dir: &Path) -> Result<u64, String> {
        crate::disk::available_bytes(dir)
    }
}

/// 备份/恢复命令后端（装饰器：其余命令透传内层）。
pub struct BackupControlBackend {
    inner: Arc<dyn IpcBackend>,
    data_dir: PathBuf,
    reads: Option<ReadPool>,
    write: Option<WriteQueue>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    space: Arc<dyn SpaceProbe>,
    capacity: CapacityConfig,
}

impl BackupControlBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        data_dir: PathBuf,
        reads: Option<ReadPool>,
        write: Option<WriteQueue>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner,
            data_dir,
            reads,
            write,
            handle,
            timeout: BACKUP_COMMAND_TIMEOUT,
            space: Arc::new(NativeSpaceProbe),
            capacity: CapacityConfig::from_env(),
        }
    }

    /// 注入空间探针（测试/故障注入：覆盖「空间不足 / 不可写」分支）。
    #[must_use]
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        self.space = space;
        self
    }

    /// 注入容量阈值（M3-05 DoD2：参数化模拟警告/强提示；语义不变）。
    #[must_use]
    pub fn with_capacity_config(mut self, capacity: CapacityConfig) -> Self {
        self.capacity = capacity;
        self
    }

    /// 当前容量阈值（诊断包与巡检复用同一注入实例）。
    pub fn capacity_config(&self) -> CapacityConfig {
        self.capacity
    }

    /// 数据库路径（数据目录 `aether.db`，M1-06 约定）。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    /// 默认备份目录（D13：应用 `backups` 目录）。
    pub fn default_backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    fn reads_required(&self) -> Result<ReadPool, IpcError> {
        self.reads.clone().ok_or_else(|| {
            IpcError::core_not_ready("备份后端未接线：read 连接池不可用（启动失败或未完成）")
        })
    }

    fn write_required(&self) -> Result<WriteQueue, IpcError> {
        self.write.clone().ok_or_else(|| {
            IpcError::core_not_ready("备份后端未接线：写队列不可用（启动失败或未完成）")
        })
    }

    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(self.timeout).map_err(|_| {
            IpcError::internal(format!(
                "备份命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }

    /// 写恢复审计（D13 第 7 步；无写队列 = 降级期，忽略不阻断恢复流程）。
    fn write_audit(&self, result: &str, resource: &Path, detail: &str) -> Result<(), IpcError> {
        let Some(write) = self.write.clone() else {
            return Ok(());
        };
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: None,
            actor: "user".to_owned(),
            action: AUDIT_ACTION_RESTORE.to_owned(),
            resource: Some(resource.to_string_lossy().to_string()),
            detail: Some(detail.to_owned()),
            result: Some(result.to_owned()),
            ts: now_ms(),
        };
        self.call(async move {
            write
                .execute(StoreCommand::InsertAudit { record })
                .await
                .map(|_| ())
                .map_err(|error| IpcError::internal(format!("恢复审计写入失败：{error}")))
        })
    }
}

impl IpcBackend for BackupControlBackend {
    fn backup_create(
        &self,
        _request: &BackupCreateRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let write = self.write_required()?;

        // 目标目录：外部（系统选择器 canonicalize 结果）或默认 `backups/`。
        let (target, kind) = match canonical_target_dir {
            Some(dir) => (dir.to_path_buf(), BACKUP_KIND_EXTERNAL),
            None => {
                let dir = self.default_backup_dir();
                std::fs::create_dir_all(&dir).map_err(|error| {
                    IpcError::internal(format!(
                        "创建默认备份目录失败（{}）：{error}",
                        dir.display()
                    ))
                })?;
                (dir, BACKUP_KIND_INTERNAL)
            }
        };

        // 空间护栏（ADR-003 决策 19）：可写 + 可用空间 ≥ 当前 db+wal ×1.2，不足拒绝并提示。
        self.space.ensure_writable(&target).map_err(|message| {
            IpcError::invalid_value(format!("{message}（backup_target_not_writable）"))
        })?;
        let db_bytes = file_len(&self.db_path());
        let wal_bytes = file_len(&wal_path(&self.db_path()));
        let required = required_space_bytes(db_bytes, wal_bytes);
        let available = self.space.available_bytes(&target).map_err(|message| {
            IpcError::invalid_value(format!(
                "目标空间探测失败（backup_space_unknown）：{message}"
            ))
        })?;
        if available < required {
            return Err(IpcError::invalid_value(format!(
                "目标可用空间不足（backup_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节（当前 db+wal 的 1.2 倍）；请选择空间充足的目录"
            )));
        }

        let created_at = now_ms();
        let id = ulid::Ulid::new().to_string();
        let file_name = aether_store::backup::backup_file_name_for(&target, created_at, &id);
        let dest = target.join(&file_name);
        let dest_for_task = dest.clone();
        let reads_for_task = reads.clone();
        let size_bytes = self.call(async move {
            reads_for_task
                .with_connection(move |connection| create_backup_file(connection, &dest_for_task))
                .await
                .map_err(|error| IpcError::internal(format!("备份写出失败：{error}")))
        })?;

        let record = BackupRecord {
            id: id.clone(),
            path: dest,
            size_bytes,
            encrypted: false,
            kind: kind.to_owned(),
            created_at,
        };

        // 登记台账（经单写队列；AGENTS §2.4）。
        let record_for_task = record.clone();
        let write_for_task = write.clone();
        self.call(async move {
            write_for_task
                .execute(StoreCommand::InsertBackup {
                    record: record_for_task,
                })
                .await
                .map(|_| ())
                .map_err(|error| IpcError::internal(format!("备份台账登记失败：{error}")))
        })?;

        // 保留策略（D13：保留最近 10 份）：删除最旧的行与文件。
        let reads_for_list = reads.clone();
        let all = self.call(async move {
            reads_for_list
                .backups()
                .await
                .map_err(|error| IpcError::internal(format!("备份清单读取失败：{error}")))
        })?;
        let mut pruned: Vec<String> = Vec::new();
        for old in prune_plan(&all, BACKUP_RETENTION) {
            let write_for_delete = write.clone();
            let old_id = old.id.clone();
            self.call(async move {
                write_for_delete
                    .execute(StoreCommand::DeleteBackup { id: old_id })
                    .await
                    .map(|_| ())
                    .map_err(|error| IpcError::internal(format!("备份台账清理失败：{error}")))
            })?;
            if old.path.exists() {
                if let Err(error) = std::fs::remove_file(&old.path) {
                    tracing::warn!(
                        path = %old.path.display(),
                        error = %error,
                        "保留策略删除备份文件失败（台账已清理）"
                    );
                }
            }
            pruned.push(old.id);
        }

        Ok(json!({
            "backup": record,
            "pruned": pruned,
        }))
    }

    fn backup_list(&self) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let all = self.call(async move {
            reads
                .backups()
                .await
                .map_err(|error| IpcError::internal(format!("备份清单读取失败：{error}")))
        })?;

        let db_bytes = file_len(&self.db_path());
        let wal_bytes = file_len(&wal_path(&self.db_path()));
        let capacity = self.capacity.to_json(db_bytes, wal_bytes);
        Ok(json!({
            "backups": all,
            "capacity": capacity,
        }))
    }

    /// D13/ADR-004：候选校验（第 1–2 步）→ 登记恢复现场日志 → 命令层请求重启；
    /// 第 3–6 步由下次启动序列执行（`boot_apply_pending_restore`）。
    fn backup_restore(
        &self,
        request: &BackupRestoreRequest,
        canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let db_path = self.db_path();

        let (candidate_path, source) = match &request.source {
            BackupSource::Internal { id } => {
                let lookup_id = id.clone();
                let reads_for_task = reads.clone();
                let record = self.call(async move {
                    reads_for_task
                        .backup(&lookup_id)
                        .await
                        .map_err(|error| IpcError::internal(format!("备份台账读取失败：{error}")))
                })?;
                let record = record.ok_or_else(|| {
                    IpcError::invalid_value(format!("内部备份不存在（backup_not_found）：{id}"))
                })?;
                (record.path, "internal")
            }
            BackupSource::External { .. } => {
                let path = canonical_external_path
                    .ok_or_else(|| {
                        IpcError::internal("外部候选路径未 canonicalize（命令层校验缺失）")
                    })?
                    .to_path_buf();
                (path, "external")
            }
        };

        // 第 2 步：候选校验（高版本拒绝并提示升级程序；损坏候选拒绝）。
        let candidate = validate_candidate(&candidate_path, program_schema_version())
            .map_err(map_backup_error)?;

        // 登记恢复请求（现场日志；执行在下次启动序列的无写者窗口）。
        request_restore(&db_path, &candidate_path, now_ms()).map_err(map_backup_error)?;
        self.write_audit(
            "requested",
            &candidate_path,
            &format!(
                "schema v{} / {} 字节；db={}",
                candidate.schema_version,
                candidate.size_bytes,
                db_path.display()
            ),
        )?;

        Ok(json!({
            "restoring": true,
            "restart_required": true,
            "source": source,
            "candidate": candidate,
            "db_path": db_path.to_string_lossy(),
        }))
    }

    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        self.inner.runtimes_list()
    }

    fn session_list(&self, request: &SessionListRequest) -> Result<Value, IpcError> {
        self.inner.session_list(request)
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<Value, IpcError> {
        self.inner.session_create(request)
    }

    fn session_send(&self, request: &SessionSendRequest) -> Result<Value, IpcError> {
        self.inner.session_send(request)
    }

    fn session_interrupt(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        self.inner.session_interrupt(request)
    }

    fn session_dispose(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        self.inner.session_dispose(request)
    }

    fn messages_page(&self, request: &MessagesPageRequest) -> Result<Value, IpcError> {
        self.inner.messages_page(request)
    }

    fn permissions_pending(&self, request: &PermissionsPendingRequest) -> Result<Value, IpcError> {
        self.inner.permissions_pending(request)
    }

    fn permission_resolve(&self, request: &PermissionResolveRequest) -> Result<Value, IpcError> {
        self.inner.permission_resolve(request)
    }

    fn settings_get(&self, request: &SettingsGetRequest) -> Result<Value, IpcError> {
        self.inner.settings_get(request)
    }

    fn settings_set(&self, request: &SettingsSetRequest) -> Result<Value, IpcError> {
        self.inner.settings_set(request)
    }

    fn app_restart(&self, request: &AppRestartRequest) -> Result<Value, IpcError> {
        self.inner.app_restart(request)
    }

    fn run_retry(&self, request: &RunRetryRequest) -> Result<Value, IpcError> {
        self.inner.run_retry(request)
    }

    fn runtime_retry(&self, request: &RuntimeRetryRequest) -> Result<Value, IpcError> {
        self.inner.runtime_retry(request)
    }

    fn runtime_enable(&self, request: &RuntimeEnableRequest) -> Result<Value, IpcError> {
        self.inner.runtime_enable(request)
    }

    fn workspace_set(
        &self,
        request: &WorkspaceSetRequest,
        canonical_root_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.workspace_set(request, canonical_root_path)
    }

    fn export_diagnostics(
        &self,
        request: &ExportDiagnosticsRequest,
        canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.export_diagnostics(request, canonical_target_dir)
    }
}

/// 存储/候选错误 → IPC 结构化错误（业务码携带在消息中，不新增错误码枚举，口径同 M3-03）。
pub fn map_backup_error(error: StoreError) -> IpcError {
    match &error {
        StoreError::SchemaNewerThanProgram { database, program } => IpcError::invalid_value(format!(
            "候选数据库 schema 版本 v{database} 高于当前程序支持的 v{program}，请升级程序（backup_candidate_newer）"
        )),
        StoreError::BackupCandidateInvalid { path, reason } => IpcError::invalid_value(format!(
            "恢复候选校验失败（backup_candidate_invalid）：{}：{reason}",
            path.display()
        )),
        StoreError::BackupNotFound { id } => {
            IpcError::invalid_value(format!("内部备份不存在（backup_not_found）：{id}"))
        }
        StoreError::BackupSpaceInsufficient {
            required,
            available,
        } => IpcError::invalid_value(format!(
            "目标可用空间不足（backup_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节"
        )),
        StoreError::RestoreJournalInvalid { path, reason } => IpcError::invalid_value(format!(
            "恢复现场日志不可用（backup_restore_journal_invalid）：{reason}（{}）",
            path.display()
        )),
        _ => IpcError::internal(format!("备份/恢复失败：{error}")),
    }
}

/// 启动序列（存储打开**之前**）处理待处理恢复：执行第 3–6 步或回滚中断现场（D13/DoD4）。
///
/// 返回 `None` = 无待处理日志；`Some(outcome)` 由调用方在核心启动后写审计。
pub fn boot_apply_pending_restore(data_dir: &Path) -> Option<RestoreStartupOutcome> {
    let db_path = data_dir.join("aether.db");
    match recover_or_apply_pending_restore(&db_path) {
        Ok(RestoreStartupOutcome::None) => None,
        Ok(outcome) => {
            tracing::info!(outcome = ?outcome, "启动序列处理恢复现场日志");
            Some(outcome)
        }
        Err(error) => {
            tracing::error!(error = %error, "启动序列处理恢复现场日志失败");
            Some(RestoreStartupOutcome::Failed {
                error: format!("启动恢复处理失败：{error}"),
            })
        }
    }
}

/// 核心启动完成后写入恢复结果审计（D13 第 7 步；写失败只告警不阻断启动）。
pub fn write_boot_restore_audit(
    write: &WriteQueue,
    handle: &tokio::runtime::Handle,
    outcome: &RestoreStartupOutcome,
) {
    let (result, detail) = match outcome {
        RestoreStartupOutcome::Applied(report) => (
            "completed",
            format!(
                "恢复完成：候选 {} / schema v{} / {}ms",
                report.candidate.path.display(),
                report.verified_schema_version,
                report.total_ms
            ),
        ),
        RestoreStartupOutcome::InterruptedRolledBack { detail } => {
            ("rolled_back", format!("恢复被中断：{detail}"))
        }
        RestoreStartupOutcome::Failed { error } => ("failed", format!("恢复失败：{error}")),
        RestoreStartupOutcome::Completed => ("completed", "恢复日志收尾（已完成）".to_owned()),
        RestoreStartupOutcome::None => return,
    };
    let record = AuditLogRecord {
        id: ulid::Ulid::new().to_string(),
        session_id: None,
        runtime_id: None,
        actor: "user".to_owned(),
        action: AUDIT_ACTION_RESTORE.to_owned(),
        resource: None,
        detail: Some(detail),
        result: Some(result.to_owned()),
        ts: now_ms(),
    };
    if let Err(error) = handle.block_on(write.execute(StoreCommand::InsertAudit { record })) {
        tracing::warn!(error = %error, "恢复结果审计写入失败（继续启动）");
    }
}

fn wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

pub(crate) fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
