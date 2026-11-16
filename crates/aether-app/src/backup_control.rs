//! 备份/恢复命令后端（M3-04；M5-02 自动备份接线；设计 D13、ADR-003 决策 19、
//! ADR-004 命令面、ADR-020）。
//!
//! 覆盖 D7 命令面中的备份族（薄适配，业务语义全部在 `aether-store` 的恢复引擎）：
//! - `backup_create`：**手动备份**（`kind='manual'`，不自动删除；M5-02 起与自动备份
//!   共用执行路径 [`crate::backup_auto::execute_backup`]：空间护栏 → `VACUUM INTO`
//!   临时文件 + `quick_check` + 原子 rename → 台账登记）；
//! - `backup_list`：备份清单（`backups` 表，最新在前）+ 容量状态（2GB/5GB，D13）
//!   + `auto` 段（M5-02/ADR-018 附录 A-5：开关/频率/保留/最近结果/连续失败/下次排期）；
//! - `backup_restore`：候选校验（内部 id / 外部 `.db`；只读打开 + `quick_check` +
//!   迁移版本检查，高版本拒绝并提示升级）→ 登记恢复现场日志（D13 七步的 1–2 步）
//!   → 命令层请求应用重启，第 3–6 步在下次启动序列（无写者窗口）执行；审计在
//!   请求与启动结果两处写入（D13 第 7 步）；
//! - 自动备份调度器（[`crate::backup_auto::AutoBackup`]）由组合根经
//!   [`BackupControlBackend::spawn_auto_backup`] 启动（不阻塞主流程）；本后端提供
//!   `backup_list.auto` 与诊断包状态投影（`backup_auto_status`）。
//!
//! 桥接口径与 `session_backend` 一致：async 存储操作 spawn 到核心运行时 +
//! `std::sync::mpsc` 同步等待（不阻塞运行时线程）。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_control::{SharedClock, SystemClock};
use aether_security::{
    decrypt_backup_file, derive_passphrase_key_with_salt, inspect_backup_artifact,
    load_or_create_master_key, BackupCryptoParams, SecretStore,
};
use aether_store::{
    AuditLogRecord, ReadPool, RestoreStartupOutcome, StoreCommand, StoreError, WriteQueue,
};
use serde_json::{json, Value};

use crate::backup_auto::{execute_backup, AutoBackup, AutoBackupDeps, BackupEncryption, BackupJob};
pub use crate::backup_auto::{BACKUP_KIND_AUTO, BACKUP_KIND_MANUAL};
use crate::diagnostics_control::BACKUP_ENCRYPTION_ENABLED_KEY;
use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    AppRestartRequest, BackupCreateRequest, BackupRestoreRequest, BackupSource,
    ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, QueueListRequest, RunRetryRequest, RuntimeEnableRequest,
    RuntimeRetryRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest, SettingsGetRequest, SettingsSetRequest, WorkspaceSetRequest,
};
use crate::ipc::error::IpcError;
use crate::ipc::{MaintenanceCell, MaintenanceState};

/// 备份命令硬超时（大库 `VACUUM INTO` 预算；与 D6 方法超时表独立，手动动作给足窗口）。
pub const BACKUP_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// 容量巡检警告阈值（D13：2GB）。
pub const CAPACITY_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 容量巡检强烈提示阈值（D13：5GB）。
pub const CAPACITY_CRITICAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// 容量阈值覆盖环境变量（M3-05 DoD2「阈值参数化模拟」；仅调参，不改变语义）。
pub const CAPACITY_WARN_ENV: &str = "AETHER_CAPACITY_WARN_BYTES";
/// 容量强提示阈值覆盖环境变量（M3-05 DoD2）。
pub const CAPACITY_CRITICAL_ENV: &str = "AETHER_CAPACITY_CRITICAL_BYTES";
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
    /// 自动/手动备份共享互斥锁（同一时刻最多 1 个备份任务；ADR-020 §2.1-5）。
    run_lock: Arc<tokio::sync::Mutex<()>>,
    /// 自动备份调度器（读写均就绪时接线；M5-02）。
    auto: Option<Arc<AutoBackup>>,
    /// M5-03/ADR-020：主密钥存储（Keychain；A3 降级为加密文件）。
    secrets: Option<Arc<dyn SecretStore>>,
    /// M5-03：口令模式 KDF 参数（生产 A3 冻结值；测试/演练注入弱参数）。
    crypto_params: BackupCryptoParams,
    /// ADR-039 D-039-5：备份/恢复维护态（组合根注入共享单元）。
    maintenance: Arc<MaintenanceCell>,
}

/// 维护态 RAII 守卫（进入备份/恢复 → 退出恢复上一状态）。
struct MaintenanceGuard {
    cell: Arc<MaintenanceCell>,
    previous: Option<MaintenanceState>,
}

impl MaintenanceGuard {
    fn enter(cell: &Arc<MaintenanceCell>, kind: &str, reason: String) -> Self {
        let previous = cell.get();
        cell.set(Some(MaintenanceState {
            kind: kind.to_owned(),
            reason,
        }));
        Self {
            cell: Arc::clone(cell),
            previous,
        }
    }
}

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        self.cell.set(self.previous.take());
    }
}

impl BackupControlBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        data_dir: PathBuf,
        reads: Option<ReadPool>,
        write: Option<WriteQueue>,
        handle: tokio::runtime::Handle,
        secrets: Option<Arc<dyn SecretStore>>,
    ) -> Self {
        let run_lock = Arc::new(tokio::sync::Mutex::new(()));
        let auto = match (reads.clone(), write.clone()) {
            (Some(reads), Some(write)) => Some(Arc::new(AutoBackup::new(AutoBackupDeps {
                data_dir: data_dir.clone(),
                reads,
                write,
                space: Arc::new(NativeSpaceProbe),
                clock: Arc::new(SystemClock),
                run_lock: Arc::clone(&run_lock),
                secrets: secrets.clone(),
                crypto_params: BackupCryptoParams::a3(),
            }))),
            _ => None,
        };
        Self {
            inner,
            data_dir,
            reads,
            write,
            handle,
            timeout: BACKUP_COMMAND_TIMEOUT,
            space: Arc::new(NativeSpaceProbe),
            capacity: CapacityConfig::from_env(),
            run_lock,
            auto,
            secrets,
            crypto_params: BackupCryptoParams::a3(),
            maintenance: Arc::new(MaintenanceCell::new()),
        }
    }

    /// 注入维护态共享单元（ADR-039 D-039-5；组合根）。
    #[must_use]
    pub fn with_maintenance_cell(mut self, cell: Arc<MaintenanceCell>) -> Self {
        self.maintenance = cell;
        self
    }

    /// 注入主密钥存储（测试/演练；组合根经构造参数接线）。
    #[must_use]
    pub fn with_secrets(mut self, secrets: Option<Arc<dyn SecretStore>>) -> Self {
        if let Some(auto) = &self.auto {
            let mut deps_auto = auto.as_ref().clone();
            deps_auto.set_secrets(secrets.clone());
            self.auto = Some(Arc::new(deps_auto));
        }
        self.secrets = secrets;
        self
    }

    /// 注入口令模式 KDF 参数（测试/演练弱参数；生产固定 A3 冻结值）。
    #[must_use]
    pub fn with_crypto_params(mut self, params: BackupCryptoParams) -> Self {
        if let Some(auto) = &self.auto {
            self.auto = Some(Arc::new(auto.as_ref().clone().with_crypto_params(params)));
        }
        self.crypto_params = params;
        self
    }

    /// 注入空间探针（测试/故障注入：覆盖「空间不足 / 不可写」分支；转发调度器）。
    #[must_use]
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        if let Some(auto) = &self.auto {
            auto.set_space_probe(Arc::clone(&space));
        }
        self.space = space;
        self
    }

    /// 注入时钟（测试：调度器排期/状态投影；M5-02 DoD1 时钟注入）。
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        if let Some(auto) = &self.auto {
            self.auto = Some(Arc::new(auto.as_ref().clone().with_clock(clock)));
        }
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

    /// 自动备份调度器句柄（测试驱动 `tick`/`run_once`；生产由组合根启动）。
    pub fn auto_backup(&self) -> Option<Arc<AutoBackup>> {
        self.auto.clone()
    }

    /// 启动自动备份后台调度器（M5-02；不阻塞主流程；`None` = 未接线）。
    pub fn spawn_auto_backup(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.auto.as_ref().map(|auto| auto.spawn(handle))
    }

    /// 当前 generation 主库路径（布局无效时退回旧扁平路径，供诊断/降级备份）。
    pub fn db_path(&self) -> PathBuf {
        aether_store::read_layout(&self.data_dir)
            .map(|layout| layout.main_db().to_path_buf())
            .unwrap_or_else(|_| self.data_dir.join("aether.db"))
    }

    fn live_capacity(&self) -> (u64, u64) {
        let Ok(layout) = aether_store::read_layout(&self.data_dir) else {
            let db = self.db_path();
            return (file_len(&db), file_len(&wal_path(&db)));
        };
        let mut main_db = 0u64;
        let mut main_wal = 0u64;
        let mut shard_db = 0u64;
        let mut shard_wal = 0u64;
        for (index, path) in layout.all_db_paths().iter().enumerate() {
            let db = file_len(path);
            let wal = file_len(&wal_path(path));
            if index == 0 {
                main_db = db;
                main_wal = wal;
            } else {
                shard_db = shard_db.saturating_add(db);
                shard_wal = shard_wal.saturating_add(wal);
            }
        }
        (
            main_db.saturating_add(shard_db),
            main_wal.saturating_add(shard_wal),
        )
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
    /// M3-04/D13 + M5-02/ADR-020 + M5-03：**手动备份**（`kind='manual'`，不自动删除）。
    ///
    /// 目标目录缺省为数据目录 `backups/`，或用户选择的外部目录（D13）；空间护栏
    /// 与原子写出（`VACUUM INTO` 暂存 + `quick_check` + 原子 rename）与自动备份共用
    /// [`execute_backup`]；与自动备份经同一互斥锁串行。
    ///
    /// 加密（M5-03/ADR-020）：`passphrase` 显式提供 → 口令模式；否则按
    /// `backup.encryption.enabled`（缺省 `true`）用 Keychain 主密钥；显式关闭才不加密。
    /// 加密开启且密钥不可用 → 失败（`backup_key_unavailable`），不产出明文。
    fn backup_create(
        &self,
        request: &BackupCreateRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let write = self.write_required()?;
        let _maintenance = MaintenanceGuard::enter(
            &self.maintenance,
            "backup",
            "备份维护中，其他功能暂不可用".to_owned(),
        );

        // 目标目录：外部（系统选择器 canonicalize 结果）或默认 `backups/`。
        let target = match canonical_target_dir {
            Some(dir) => dir.to_path_buf(),
            None => {
                let dir = self.default_backup_dir();
                std::fs::create_dir_all(&dir).map_err(|error| {
                    IpcError::internal(format!(
                        "创建默认备份目录失败（{}）：{error}",
                        dir.display()
                    ))
                })?;
                dir
            }
        };

        // M5-03：加密设置（缺省开启；读取失败按缺省语义处理）。
        let encryption_enabled = {
            let reads_for_setting = reads.clone();
            self.call(async move {
                reads_for_setting
                    .setting(BACKUP_ENCRYPTION_ENABLED_KEY)
                    .await
                    .map_err(|error| IpcError::internal(format!("设置读取失败：{error}")))
            })?
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
        };
        let passphrase = request.passphrase_value();

        let created_at = now_ms();
        let id = ulid::Ulid::new().to_string();
        let data_dir = self.data_dir.clone();
        let space = Arc::clone(&self.space);
        let lock = Arc::clone(&self.run_lock);
        let secrets = self.secrets.clone();
        let crypto_params = self.crypto_params;
        let outcome = self.call(async move {
            let encryption = match &passphrase {
                Some(passphrase) => BackupEncryption::Passphrase(passphrase),
                None if encryption_enabled => BackupEncryption::Keychain,
                None => BackupEncryption::None,
            };
            let job = BackupJob {
                target: &target,
                kind: BACKUP_KIND_MANUAL,
                auto_retention: None,
                created_at,
                id: &id,
                encryption,
                secrets: secrets.as_deref(),
                crypto_params,
            };
            execute_backup(&reads, &write, &data_dir, &lock, space.as_ref(), &job).await
        })?;

        Ok(json!({
            "backup": outcome.record,
            "pruned": outcome.pruned,
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
        let all: Vec<_> = all.into_iter().map(enrich_backup_record).collect();

        let (db_bytes, wal_bytes) = self.live_capacity();
        let capacity = self.capacity.to_json(db_bytes, wal_bytes);
        let auto = match &self.auto {
            Some(auto) => {
                let auto = Arc::clone(auto);
                Some(self.call(async move { auto.auto_segment().await })?)
            }
            None => None,
        };
        let mut value = json!({
            "backups": all,
            "capacity": capacity,
        });
        if let Some(auto) = auto {
            if let Some(object) = value.as_object_mut() {
                object.insert("auto".to_owned(), auto);
            }
        }
        Ok(value)
    }

    /// M5-02（诊断包 `backup_auto` 段）：自动备份运行状态（内部接线，非 IPC 命令）。
    fn backup_auto_status(&self) -> Result<Option<Value>, IpcError> {
        match &self.auto {
            Some(auto) => {
                let auto = Arc::clone(auto);
                self.call(async move { auto.status().await }).map(Some)
            }
            None => Ok(None),
        }
    }

    /// ADR-039 D-039-5 §4/D-039-8：整套备份恢复（外部路径仅接受备份集目录或
    /// `manifest.json`；旧 `.db`/`.aebk` 单文件由 `validate_backup_set` 拒绝）。
    ///
    /// 校验 → staging 空间预检（≥ generation ×1.1）→ 同卷 staging generation →
    /// `restore-journal.json`；`CURRENT` 原子切换由下次启动序列执行。
    fn backup_restore(
        &self,
        request: &BackupRestoreRequest,
        canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let _reads = self.reads_required()?;
        let _maintenance = MaintenanceGuard::enter(
            &self.maintenance,
            "restore",
            "恢复维护中，其他功能暂不可用".to_owned(),
        );
        let _write_gate = match self.write.clone() {
            Some(write) => Some(self.call(async move {
                write
                    .pause_and_drain()
                    .await
                    .map_err(|error| IpcError::internal(format!("恢复维护态 drain 失败：{error}")))
            })?),
            None => None,
        };
        let (candidate_path, source, source_backup_id) = match &request.source {
            BackupSource::Internal { id } => {
                let lookup_id = id.clone();
                let reads_for_task = self.reads_required()?;
                let record = self.call(async move {
                    reads_for_task
                        .backup(&lookup_id)
                        .await
                        .map_err(|error| IpcError::internal(format!("备份台账读取失败：{error}")))
                })?;
                let record = record.ok_or_else(|| {
                    IpcError::invalid_value(format!("内部备份不存在（backup_not_found）：{id}"))
                })?;
                (record.path, "internal", Some(id.clone()))
            }
            BackupSource::External { .. } => {
                let path = canonical_external_path
                    .ok_or_else(|| {
                        IpcError::internal("外部候选路径未 canonicalize（命令层校验缺失）")
                    })?
                    .to_path_buf();
                (path, "external", None)
            }
        };

        let set = aether_store::validate_backup_set(&candidate_path).map_err(map_backup_error)?;
        if set.manifest.shard_count != aether_store::SHARD_COUNT {
            return Err(IpcError::invalid_value(format!(
                "恢复候选校验失败（backup_candidate_invalid）：分片数 {}（期望 {}）",
                set.manifest.shard_count,
                aether_store::SHARD_COUNT
            )));
        }
        // ADR-039 D-039-5 §5.3：复制前预检 `<data_dir>/staging` 可用空间 ≥ generation ×1.1。
        let required = aether_store::restore_space_required(set.total_bytes);
        let available = self
            .space
            .available_bytes(&self.data_dir)
            .map_err(|message| IpcError::internal(format!("恢复空间探测失败：{message}")))?;
        if available < required {
            return Err(IpcError::invalid_value(format!(
                "恢复空间不足（backup_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节（备份 generation ×1.1）；当前数据未改变"
            )));
        }

        let new_generation = aether_store::new_generation_id();
        let requested_at = now_ms();
        if set.manifest.artifact_mode == "passphrase" && request.passphrase_value().is_none() {
            return Err(IpcError::invalid_value(
                "该备份为口令加密产物，恢复需要提供口令（backup_passphrase_required）",
            ));
        }
        let staged = if set.manifest.encrypted {
            let master = set_master_key(request, self.secrets.as_deref())?;
            let crypto_params = self.crypto_params;
            let passphrase = request.passphrase_value();
            aether_store::stage_restore_with(
                &self.data_dir,
                &set,
                &new_generation,
                requested_at,
                |source_path, dest| {
                    decrypt_set_file(
                        source_path,
                        dest,
                        master.as_ref(),
                        passphrase.as_ref(),
                        crypto_params,
                    )
                },
            )
        } else {
            aether_store::stage_restore(&self.data_dir, &set, &new_generation, requested_at)
        };
        let journal = match staged {
            Ok(journal) => journal,
            Err(error) => {
                let staging =
                    aether_store::staging_generation(&self.data_dir, "restore", &new_generation);
                if let Some(job_dir) = staging.parent() {
                    let _ = std::fs::remove_dir_all(job_dir);
                }
                let _ = std::fs::remove_dir_all(staging);
                return Err(map_restore_error(error));
            }
        };
        if let Err(error) = aether_store::write_generation_journal(&self.data_dir, &journal) {
            let staging =
                aether_store::staging_generation(&self.data_dir, "restore", &new_generation);
            if let Some(job_dir) = staging.parent() {
                let _ = std::fs::remove_dir_all(job_dir);
            }
            let _ = std::fs::remove_dir_all(staging);
            return Err(map_restore_error(error));
        }
        // staging/journal 已完成：恢复写队列后再写审计（审计写入需要写队列）。
        drop(_write_gate);

        self.write_audit(
            "requested",
            &set.root,
            &format!(
                "sharded_set schema v{} / {} 字节 / {} 分片；generation={}；{}",
                set.manifest.schema_version,
                set.total_bytes,
                set.manifest.shard_count,
                new_generation,
                aether_store::RESTORE_ATTACHMENTS_NOTICE
            ),
        )?;

        Ok(json!({
            "restoring": true,
            "restart_required": true,
            "source": source,
            "candidate": {
                "path": set.root.to_string_lossy(),
                "set_path": set.root.to_string_lossy(),
                "manifest_path": set.manifest_path.to_string_lossy(),
                "manifest_sha256": set.manifest_sha256,
                "size_bytes": set.total_bytes,
                "schema_version": set.manifest.schema_version,
                "encrypted": set.manifest.encrypted,
                "artifact_mode": set.manifest.artifact_mode,
                "source_backup_id": source_backup_id,
                "format": aether_store::BACKUP_FORMAT_SHARDED_SET,
                "shard_count": set.manifest.shard_count,
                "generation": new_generation,
                "attachments_notice": aether_store::RESTORE_ATTACHMENTS_NOTICE,
            },
            "db_path": self.db_path().to_string_lossy(),
        }))
    }

    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    fn queue_list(&self, request: &QueueListRequest) -> Result<Value, IpcError> {
        self.inner.queue_list(request)
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

/// 加密备份集主密钥（Keychain 模式；口令模式返回 `None`，逐文件派生）。
fn set_master_key(
    request: &BackupRestoreRequest,
    secrets: Option<&dyn SecretStore>,
) -> Result<Option<[u8; aether_security::KEY_LEN]>, IpcError> {
    match request.passphrase_value() {
        Some(_) => Ok(None),
        None => {
            let store = secrets.ok_or_else(|| {
                IpcError::invalid_value(
                    "主密钥不可用（backup_key_unavailable）：OS 凭据库不可用且未解锁 A3 降级口令；加密备份集需要主密钥或口令",
                )
            })?;
            let master = load_or_create_master_key(store).map_err(|error| {
                IpcError::invalid_value(format!("主密钥不可用（backup_key_unavailable）：{error}"))
            })?;
            Ok(Some(*master))
        }
    }
}

/// 解密备份集单个文件（Keychain 主密钥或逐文件口令盐派生）。
fn decrypt_set_file(
    source: &Path,
    dest: &Path,
    master: Option<&[u8; aether_security::KEY_LEN]>,
    passphrase: Option<&aether_security::SecretValue>,
    crypto_params: BackupCryptoParams,
) -> Result<(), StoreError> {
    let key = match master {
        Some(master) => *master,
        None => {
            let passphrase = passphrase.ok_or_else(|| StoreError::Internal {
                reason: "该备份为口令加密集合，恢复需要提供口令（backup_passphrase_required）"
                    .to_owned(),
            })?;
            let info = inspect_backup_artifact(source).map_err(|error| StoreError::Internal {
                reason: format!("加密产物头非法：{error}"),
            })?;
            let salt = info.salt.ok_or_else(|| StoreError::Internal {
                reason: "口令模式产物缺少盐".to_owned(),
            })?;
            let derived = derive_passphrase_key_with_salt(crypto_params, &salt, passphrase)
                .map_err(|error| StoreError::Internal {
                    reason: format!("恢复口令派生失败：{error}"),
                })?;
            *derived
        }
    };
    decrypt_backup_file(source, dest, &key)
        .map(|_| ())
        .map_err(|error| StoreError::Internal {
            reason: format!("备份解密失败（backup_decrypt_failed）：{error}"),
        })
}

/// 备份记录富化（ADR-039 D-039-5 §4）：目录备份集读取 manifest 补充
/// `format/set_path/manifest_path/manifest_sha256/shard_count/integrity`；
/// 旧单文件记录标记 `single_file`（仅展示，UI 恢复禁用）。
fn enrich_backup_record(mut record: aether_store::BackupRecord) -> aether_store::BackupRecord {
    if record.path.is_dir() {
        let manifest_path = record.path.join(aether_store::MANIFEST_FILE_NAME);
        if manifest_path.is_file() {
            if let Ok(bytes) = std::fs::read(&manifest_path) {
                if let Ok(manifest) =
                    serde_json::from_slice::<aether_store::BackupSetManifest>(&bytes)
                {
                    record.format = Some(aether_store::BACKUP_FORMAT_SHARDED_SET.to_owned());
                    record.set_path = Some(record.path.clone());
                    record.manifest_path = Some(manifest_path);
                    record.manifest_sha256 = Some(sha256_hex(&bytes));
                    record.shard_count = Some(manifest.shard_count);
                    record.integrity = Some(aether_store::BackupIntegrity {
                        status: "ok".to_owned(),
                        checked_at: None,
                    });
                    return record;
                }
            }
        }
    }
    record.format = Some(aether_store::BACKUP_FORMAT_SINGLE_FILE.to_owned());
    record.integrity = Some(aether_store::BackupIntegrity {
        status: "unverified".to_owned(),
        checked_at: None,
    });
    record
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 恢复错误映射：解密/口令失败保留业务码（`backup_decrypt_failed`/
/// `backup_passphrase_required`），其余沿用 [`map_backup_error`]。
fn map_restore_error(error: StoreError) -> IpcError {
    if let StoreError::Internal { reason } = &error {
        if reason.contains("backup_decrypt_failed") || reason.contains("backup_passphrase_required")
        {
            return IpcError::invalid_value(reason.clone());
        }
    }
    map_backup_error(error)
}

/// 启动序列（存储打开**之前**）处理待处理恢复（ADR-039 D-039-8 整套切换）：
/// 校验 staging generation → 原子写 `CURRENT` → 旧 generation 保留 `.pre-restore-*`
/// → 清理 journal；失败整套回滚/保留 journal，当前库不被损。
///
/// 加密解密已在 `backup_restore` 命令阶段完成（staging generation 为明文）。
/// 返回 `None` = 无待处理 journal；`Some(outcome)` 由调用方在核心启动后写审计。
pub fn boot_apply_pending_restore(data_dir: &Path) -> Option<RestoreStartupOutcome> {
    let journal = match aether_store::read_generation_journal(data_dir, "restore") {
        Ok(Some(journal)) => journal,
        Ok(None) => return None,
        Err(error) => {
            tracing::error!(error = %error, "恢复 journal 不可读");
            return Some(RestoreStartupOutcome::Failed {
                error: format!("恢复 journal 不可用：{error}"),
            });
        }
    };
    let started = std::time::Instant::now();
    if let Err(error) =
        aether_store::verify_staged_generation(data_dir, "restore", &journal.new_generation)
    {
        tracing::error!(error = %error, "恢复 staging generation 校验失败（保留当前库）");
        return Some(RestoreStartupOutcome::Failed {
            error: format!("恢复 staging 校验失败：{error}"),
        });
    }
    let pointer =
        match aether_store::commit_staged_generation(data_dir, "restore", &journal, now_ms()) {
            Ok(pointer) => pointer,
            Err(error) => {
                tracing::error!(error = %error, "恢复 CURRENT 切换失败（整套回滚至旧 generation）");
                return Some(RestoreStartupOutcome::Failed {
                    error: format!("恢复切换失败：{error}"),
                });
            }
        };
    let _ = aether_store::remove_generation_journal(data_dir, "restore");
    let schema_version = aether_store::read_layout(data_dir)
        .ok()
        .and_then(|layout| aether_store::preflight(layout.main_db()).ok())
        .map(|report| report.schema_version)
        .unwrap_or(0);
    Some(RestoreStartupOutcome::Applied(
        aether_store::RestoreReport {
            candidate: aether_store::CandidateReport {
                path: pointer.generation.clone().into(),
                size_bytes: 0,
                schema_version,
            },
            scene: None,
            applied: true,
            rolled_back: false,
            verified_schema_version: schema_version,
            total_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            steps: Vec::new(),
        },
    ))
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

pub(crate) fn wal_path(db_path: &Path) -> PathBuf {
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
