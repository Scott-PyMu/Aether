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
    is_encrypted_backup_artifact, load_or_create_master_key, BackupArtifactMode,
    BackupCryptoParams, SecretError, SecretStore,
};
use aether_store::backup::{
    cleanup_restore_candidates, program_schema_version, read_restore_journal, request_restore,
    validate_candidate, RESTORE_CANDIDATE_INFIX,
};
use aether_store::{
    recover_or_apply_pending_restore, AuditLogRecord, ReadPool, RestoreStartupOutcome,
    StoreCommand, StoreError, WriteQueue,
};
use serde_json::{json, Value};

use crate::backup_auto::{execute_backup, AutoBackup, AutoBackupDeps, BackupEncryption, BackupJob};
pub use crate::backup_auto::{BACKUP_KIND_AUTO, BACKUP_KIND_MANUAL};
use crate::diagnostics_control::BACKUP_ENCRYPTION_ENABLED_KEY;
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
        }
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

    /// M5-03：加密产物恢复路径（解密 → 第 2 步候选校验 → 现场日志登记）。
    ///
    /// 失败路径（口令/密钥错误、头篡改、候选非法）在登记现场日志之前返回，
    /// **当前库三件套不被触碰**（ADR-020 §2.1-4）。
    fn restore_encrypted_candidate(
        &self,
        request: &BackupRestoreRequest,
        candidate_path: &Path,
        source: &str,
        db_path: &Path,
    ) -> Result<Value, IpcError> {
        let info = inspect_backup_artifact(candidate_path).map_err(|error| {
            IpcError::invalid_value(format!(
                "恢复候选校验失败（backup_candidate_invalid）：加密产物头非法：{error}"
            ))
        })?;

        // 来源 id 交叉校验（ADR-020 附录 A-2：「来源 id 记入产物头（恢复校验）」）。
        if let BackupSource::Internal { id } = &request.source {
            if info.source_backup_id != *id {
                return Err(IpcError::invalid_value(format!(
                    "恢复候选校验失败（backup_candidate_invalid）：产物来源 id（{}）与台账 id（{id}）不一致",
                    info.source_backup_id
                )));
            }
        }

        let passphrase = request.passphrase_value();
        let key = match info.mode {
            BackupArtifactMode::Passphrase => {
                let Some(passphrase) = passphrase else {
                    return Err(IpcError::invalid_value(
                        "该备份为口令加密产物，恢复需要提供口令（backup_passphrase_required）",
                    ));
                };
                let Some(salt) = info.salt else {
                    return Err(IpcError::invalid_value(
                        "恢复候选校验失败（backup_candidate_invalid）：口令模式产物缺少盐",
                    ));
                };
                derive_passphrase_key_with_salt(self.crypto_params, &salt, &passphrase)
                    .map_err(|error| IpcError::internal(format!("恢复口令派生失败：{error}")))?
            }
            BackupArtifactMode::Keychain => {
                let store = self.secrets.as_deref().ok_or_else(|| {
                    IpcError::invalid_value(
                        "主密钥不可用（backup_key_unavailable）：OS 凭据库不可用且未解锁 A3 降级口令；仅口令模式产物可用口令恢复",
                    )
                })?;
                load_or_create_master_key(store).map_err(|error| {
                    IpcError::invalid_value(format!(
                        "主密钥不可用（backup_key_unavailable）：{error}"
                    ))
                })?
            }
        };

        // 解密到候选临时文件（明文；随启动序列消费后清理；口令不落盘）。
        let decrypted = self.decrypted_candidate_path(db_path);
        if decrypted.exists() {
            let _ = std::fs::remove_file(&decrypted);
        }
        if let Err(error) = decrypt_backup_file(candidate_path, &decrypted, &key) {
            let _ = std::fs::remove_file(&decrypted);
            return Err(map_decrypt_error(error));
        }

        let candidate = match validate_candidate(&decrypted, program_schema_version()) {
            Ok(candidate) => candidate,
            Err(error) => {
                let _ = std::fs::remove_file(&decrypted);
                return Err(map_backup_error(error));
            }
        };
        if let Err(error) = request_restore(db_path, &decrypted, now_ms()) {
            let _ = std::fs::remove_file(&decrypted);
            return Err(map_backup_error(error));
        }
        self.write_audit(
            "requested",
            candidate_path,
            &format!(
                "encrypted({}); source_backup_id={}; schema v{} / {} 字节；db={}",
                artifact_mode_str(info.mode),
                info.source_backup_id,
                candidate.schema_version,
                candidate.size_bytes,
                db_path.display()
            ),
        )?;

        Ok(json!({
            "restoring": true,
            "restart_required": true,
            "source": source,
            "candidate": {
                "path": candidate.path.to_string_lossy(),
                "size_bytes": candidate.size_bytes,
                "schema_version": candidate.schema_version,
                "encrypted": true,
                "artifact_mode": artifact_mode_str(info.mode),
                "source_backup_id": info.source_backup_id,
            },
            "db_path": db_path.to_string_lossy(),
        }))
    }

    /// 解密候选临时文件路径（`<db>.restore-cand-<ts>`；同目录，随恢复收尾清理）。
    fn decrypted_candidate_path(&self, db_path: &Path) -> PathBuf {
        let name = db_path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "aether.db".to_owned());
        db_path.with_file_name(format!("{name}{RESTORE_CANDIDATE_INFIX}{}", now_ms()))
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
        let db_path = self.db_path();
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
            execute_backup(&reads, &write, &db_path, &lock, space.as_ref(), &job).await
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

        let db_bytes = file_len(&self.db_path());
        let wal_bytes = file_len(&wal_path(&self.db_path()));
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

    /// D13/ADR-004 + M5-03：候选校验（第 1–2 步）→ 登记恢复现场日志 → 命令层请求重启；
    /// 第 3–6 步由下次启动序列执行（`boot_apply_pending_restore`）。
    ///
    /// 加密产物（`AEBK1`，ADR-020 附录 A-2）：按产物头模式解密——口令模式要求
    /// `passphrase`（缺省 → `backup_passphrase_required`）；主密钥模式经安全存储读取
    /// `keychain://aether/backup/master`（不可用 → `backup_key_unavailable`）。解密出的
    /// 明文候选（`aether.db.restore-cand-<ts>`）经第 2 步校验后随现场日志登记，
    /// 由启动序列消费；口令/密钥错误（tag 校验失败）→ `backup_decrypt_failed`，
    /// **现场三件套不被破坏**（本路径不触碰当前库）。
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

        // M5-03：产物形态判定（magic 优先；明文 `.db` 保持 P0 路径）。
        if is_encrypted_backup_artifact(&candidate_path) {
            return self.restore_encrypted_candidate(request, &candidate_path, source, &db_path);
        }

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
            "candidate": {
                "path": candidate.path.to_string_lossy(),
                "size_bytes": candidate.size_bytes,
                "schema_version": candidate.schema_version,
                "encrypted": false,
                "artifact_mode": Value::Null,
                "source_backup_id": Value::Null,
            },
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

/// M5-03：解密失败 → 结构化错误（`backup_decrypt_failed`；不含口令/密钥值）。
fn map_decrypt_error(error: SecretError) -> IpcError {
    match error {
        SecretError::Decrypt => IpcError::invalid_value(
            "恢复口令或主密钥错误（backup_decrypt_failed）：产物认证失败（口令/密钥错误或产物被篡改）；当前数据未受影响",
        ),
        other => IpcError::internal(format!("恢复解密失败（backup_decrypt_failed）：{other}")),
    }
}

/// 产物模式 → 稳定文本（响应/审计/证据）。
fn artifact_mode_str(mode: BackupArtifactMode) -> &'static str {
    match mode {
        BackupArtifactMode::Keychain => "keychain",
        BackupArtifactMode::Passphrase => "passphrase",
    }
}

/// 启动序列（存储打开**之前**）处理待处理恢复：执行第 3–6 步或回滚中断现场（D13/DoD4）。
///
/// M5-03：加密产物恢复会在库目录留下明文候选（`<db>.restore-cand-*`）。启动时先
/// 清理历史残留（保留现场日志引用的候选），处理结束后删除被引用的候选文件。
///
/// 返回 `None` = 无待处理日志；`Some(outcome)` 由调用方在核心启动后写审计。
pub fn boot_apply_pending_restore(data_dir: &Path) -> Option<RestoreStartupOutcome> {
    let db_path = data_dir.join("aether.db");
    let pending = read_restore_journal(&db_path).ok().flatten();
    let keep = pending
        .as_ref()
        .map(|journal| PathBuf::from(&journal.candidate_path));
    match cleanup_restore_candidates(&db_path, keep.as_deref()) {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "清理历史解密候选残留（M5-03）"),
        Err(error) => tracing::warn!(error = %error, "解密候选残留清理失败（继续启动）"),
    }

    let outcome = match recover_or_apply_pending_restore(&db_path) {
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
    };

    // 被现场日志引用的解密候选已消费/已回滚：删除（仅限本机制生成的临时候选）。
    if let Some(keep) = keep {
        let is_candidate_temp = keep
            .file_name()
            .map(|name| name.to_string_lossy().contains(RESTORE_CANDIDATE_INFIX))
            .unwrap_or(false);
        if is_candidate_temp && keep.exists() {
            if let Err(error) = std::fs::remove_file(&keep) {
                tracing::warn!(path = %keep.display(), error = %error, "解密候选清理失败");
            }
        }
    }
    outcome
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
