//! 诊断导出与容量巡检命令后端（M3-05；设计 D11/D13、ADR-007 增量、ADR-003 决策 19）。
//!
//! 覆盖：
//! - `export_diagnostics`：诊断包导出（脱敏配置 + 版本 + 健康快照 + 库摘要 + 容量 +
//!   **运行期日志汇聚产物**（含写失败 `attempt=n/3` 与 `persist_degraded` 诊断）+
//!   任务 dump（M2-05 缓冲））；整包经 [`aether_security::Redactor`] 递归脱敏，
//!   写出前后各校验一次「密钥模式 0 命中」（D10；`sk-`/`eyJ`/PEM 家族）；
//! - `backup_list` 增强：附加 `reminder` 段（D13「7 天未备份提醒（可关闭）」，
//!   时钟可注入；开关持久化于 `settings` 表）；
//! - `settings_get`/`settings_set`：已登记设置键（当前：`backup.reminder`）经
//!   单写队列持久化（AGENTS §2.4）。
//!
//! 容量阈值（2GB/5GB，D13）经 [`CapacityConfig`] 参数化注入（M3-05 DoD2 模拟）；
//! 本模块不新增命令、不新增事件、不新增迁移。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aether_control::{SharedClock, SystemClock};
use aether_security::Redactor;
use aether_store::backup::required_space_bytes;
use aether_store::{ReadPool, StoreCommand, StoreSummary, WriteQueue};
use serde_json::{json, Value};

use crate::backup_control::{file_len, CapacityConfig, SpaceProbe, BACKUP_COMMAND_TIMEOUT};
use crate::core_health::HealthProvider;
use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    AppRestartRequest, BackupCreateRequest, BackupRestoreRequest, ExportDiagnosticsRequest,
    MessagesPageRequest, PermissionResolveRequest, PermissionsPendingRequest, RunRetryRequest,
    RuntimeEnableRequest, RuntimeRetryRequest, SessionCreateRequest, SessionIdRequest,
    SessionListRequest, SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
    WorkspaceSetRequest,
};
use crate::ipc::error::{IpcError, IpcErrorCode};
use crate::logging::LogSink;

/// 设置键：备份提醒开关（M3-05 登记；UI-UX Q9/Q10：全局开关，缺省开启）。
pub const BACKUP_REMINDER_KEY: &str = "backup.reminder";
/// 未备份提醒阈值（D13：7 天）。
pub const BACKUP_REMINDER_THRESHOLD_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 诊断包文件名前缀（`aether-diagnostics-<ts>.json`）。
pub const DIAGNOSTICS_FILE_PREFIX: &str = "aether-diagnostics";
/// 诊断包 stdout 冒烟 CLI 参数（M1-01 `--aether-diagnostics` 不受影响）。
pub const DIAGNOSTICS_BUNDLE_VERSION: u32 = 1;

/// 任务 dump 来源（M2-05 诊断缓冲；生产为 [`aether_control::SessionManager`]）。
pub trait TaskDumpSource: Send + Sync + 'static {
    /// 序列化后的 dump 列表（最旧在前）。
    fn task_dumps(&self) -> Vec<Value>;
}

impl TaskDumpSource for aether_control::SessionManager {
    fn task_dumps(&self) -> Vec<Value> {
        let dumps = aether_control::SessionManager::task_dumps(self);
        dumps
            .into_iter()
            .filter_map(|dump| serde_json::to_value(&dump).ok())
            .collect()
    }
}

/// 诊断后端依赖（组合根构造；降级启动时读/写/任务源为 `None`）。
pub struct DiagnosticsDeps {
    pub data_dir: PathBuf,
    pub reads: Option<ReadPool>,
    pub write: Option<WriteQueue>,
    pub handle: tokio::runtime::Handle,
    pub health: HealthProvider,
    pub logs: Option<Arc<LogSink>>,
    pub task_dumps: Option<Arc<dyn TaskDumpSource>>,
    /// 安全级别探针（`None` = 生产原生探针；测试注入固定值避免真实凭据库调用）。
    pub security: Option<Arc<dyn crate::security_level::SecurityProbe>>,
}

/// 诊断/容量后端（装饰器：其余命令透传内层）。
pub struct DiagnosticsControlBackend {
    inner: Arc<dyn IpcBackend>,
    data_dir: PathBuf,
    reads: Option<ReadPool>,
    write: Option<WriteQueue>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    clock: SharedClock,
    capacity: CapacityConfig,
    space: Arc<dyn SpaceProbe>,
    logs: Option<Arc<LogSink>>,
    task_dumps: Option<Arc<dyn TaskDumpSource>>,
    health: HealthProvider,
    security: Arc<dyn crate::security_level::SecurityProbe>,
}

impl DiagnosticsControlBackend {
    pub fn new(inner: Arc<dyn IpcBackend>, deps: DiagnosticsDeps) -> Self {
        Self {
            inner,
            data_dir: deps.data_dir,
            reads: deps.reads,
            write: deps.write,
            handle: deps.handle,
            timeout: BACKUP_COMMAND_TIMEOUT,
            clock: Arc::new(SystemClock),
            capacity: CapacityConfig::from_env(),
            space: Arc::new(crate::backup_control::NativeSpaceProbe),
            logs: deps.logs,
            task_dumps: deps.task_dumps,
            health: deps.health,
            security: deps
                .security
                .unwrap_or_else(|| Arc::new(crate::security_level::NativeSecurityProbe)),
        }
    }

    /// 注入时钟（M3-05 DoD3：7 天提醒/可控时钟；测试与演练）。
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// 注入容量阈值（M3-05 DoD2：参数化模拟警告/强提示）。
    #[must_use]
    pub fn with_capacity_config(mut self, capacity: CapacityConfig) -> Self {
        self.capacity = capacity;
        self
    }

    /// 注入空间探针（ADR-003 决策 19：导出外部路径的空间护栏可测试）。
    #[must_use]
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        self.space = space;
        self
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    fn wal_path(&self) -> PathBuf {
        PathBuf::from(format!("{}-wal", self.db_path().display()))
    }

    fn reads_required(&self) -> Result<ReadPool, IpcError> {
        self.reads.clone().ok_or_else(|| {
            IpcError::core_not_ready("诊断后端未接线：read 连接池不可用（启动失败或未完成）")
        })
    }

    fn write_required(&self) -> Result<WriteQueue, IpcError> {
        self.write.clone().ok_or_else(|| {
            IpcError::core_not_ready("设置写入不可用：写队列未接线（启动失败或未完成）")
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
                "诊断命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }

    /// 设置键登记校验（后端侧兜底；DTO 层已先行拒绝未登记键）。
    fn ensure_registered_key(key: &str) -> Result<(), IpcError> {
        if crate::ipc::dto::SETTINGS_KEY_ALLOWLIST.contains(&key) {
            return Ok(());
        }
        Err(IpcError::invalid_enum(
            "key",
            format!(
                "未登记的设置键（当前白名单：{:?}）",
                crate::ipc::dto::SETTINGS_KEY_ALLOWLIST
            ),
        ))
    }

    /// 键值语义校验（当前仅 `backup.reminder`：必须为 bool）。
    fn validate_setting_value(key: &str, value: &Value) -> Result<(), IpcError> {
        if key == BACKUP_REMINDER_KEY && !value.is_boolean() {
            return Err(IpcError::new(
                IpcErrorCode::InvalidType,
                format!("设置键 {key} 的值必须为布尔（true/false）"),
            ));
        }
        Ok(())
    }

    /// 读取设置值（缺省语义：`backup.reminder` 缺省为 `true`）。
    fn setting_value(&self, key: &str) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let lookup = key.to_owned();
        let raw = self.call(async move {
            reads
                .setting(&lookup)
                .await
                .map_err(|error| IpcError::internal(format!("设置读取失败：{error}")))
        })?;
        Ok(match raw {
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => value,
                Err(error) => {
                    // 仅可能来自外部改库（写入路径恒为 JSON）；按缺省语义处理并留诊断。
                    tracing::warn!(key = %key, error = %error, "设置值不是合法 JSON：按缺省语义处理");
                    default_setting_value(key)
                }
            },
            None => default_setting_value(key),
        })
    }

    /// `backup_list` 的 `reminder` 段（D13；时钟注入 + 开关可关闭）。
    fn reminder(&self) -> Result<Value, IpcError> {
        let enabled = match self.setting_value(BACKUP_REMINDER_KEY)? {
            Value::Bool(enabled) => enabled,
            other => {
                tracing::warn!(
                    value = %other,
                    "设置 {} 不是布尔值：按缺省语义（开启）处理",
                    BACKUP_REMINDER_KEY
                );
                true
            }
        };
        let last_backup_at = match &self.reads {
            Some(reads) => {
                let reads = reads.clone();
                let backups = self.call(async move {
                    reads
                        .backups()
                        .await
                        .map_err(|error| IpcError::internal(format!("备份清单读取失败：{error}")))
                })?;
                backups.first().map(|record| record.created_at)
            }
            None => None,
        };
        let since_ms = last_backup_at.map(|ts| self.clock.now_ms().saturating_sub(ts));
        let due = enabled
            && since_ms
                .map(|since| since >= BACKUP_REMINDER_THRESHOLD_MS)
                .unwrap_or(true);
        let reason = if !enabled {
            None
        } else if last_backup_at.is_none() {
            Some("never")
        } else if due {
            Some("stale")
        } else {
            None
        };
        Ok(json!({
            "enabled": enabled,
            "due": due,
            "last_backup_at": last_backup_at,
            "since_ms": since_ms,
            "threshold_ms": BACKUP_REMINDER_THRESHOLD_MS,
            "reason": reason,
        }))
    }

    /// 组装诊断包（未脱敏；调用方负责 [`Redactor::redact_json`]）。
    fn collect_bundle(&self, generated_at: i64) -> Result<(Value, Vec<String>), IpcError> {
        let mut notes: Vec<String> = Vec::new();
        let db_bytes = file_len(&self.db_path());
        let wal_bytes = file_len(&self.wal_path());

        let store: Option<StoreSummary> = match &self.reads {
            Some(reads) => {
                let reads = reads.clone();
                Some(self.call(async move {
                    reads
                        .store_summary()
                        .await
                        .map_err(|error| IpcError::internal(format!("库摘要读取失败：{error}")))
                })?)
            }
            None => {
                notes.push("读连接池不可用：库摘要与配置段省略（降级/启动失败态）".to_owned());
                None
            }
        };

        let settings = match &self.reads {
            Some(reads) => {
                let reads = reads.clone();
                let rows = self.call(async move {
                    reads
                        .settings()
                        .await
                        .map_err(|error| IpcError::internal(format!("设置读取失败：{error}")))
                })?;
                let mut map = serde_json::Map::new();
                for (key, value) in rows {
                    let parsed =
                        serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
                    map.insert(key, parsed);
                }
                Value::Object(map)
            }
            None => Value::Null,
        };

        let logs = match &self.logs {
            Some(sink) => json!({
                "line_count": sink.line_count(),
                "capacity": crate::logging::DEFAULT_RING_CAPACITY,
                "file": sink.file_path().map(|path| path.to_string_lossy().to_string()),
                "text": sink.export_text(),
            }),
            None => {
                notes.push("日志汇聚端不可用：日志段省略".to_owned());
                Value::Null
            }
        };

        let task_dumps = match &self.task_dumps {
            Some(source) => Value::Array(source.task_dumps()),
            None => {
                notes.push("任务 dump 源不可用：dump 段省略".to_owned());
                Value::Array(Vec::new())
            }
        };

        let security = self.security.status();
        let bundle = json!({
            "bundle_version": DIAGNOSTICS_BUNDLE_VERSION,
            "generated_at": generated_at,
            "app": {
                "name": crate::APP_NAME,
                "version": crate::version(),
                "core_version": crate::core_version(),
                "protocol_version": aether_adapters::protocol::PROTOCOL_VERSION,
            },
            "host": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "data_dir": self.data_dir.to_string_lossy(),
            },
            "health": self.health.report(),
            "security": {
                "level": security.level,
                "detail": security.detail,
            },
            "capacity": self.capacity.to_json(db_bytes, wal_bytes),
            "store": store,
            "config": { "settings": settings },
            "logs": logs,
            "task_dumps": task_dumps,
            "notes": notes,
        });
        Ok((bundle, notes))
    }

    /// 诊断包文件名（同毫秒冲突时追加 ULID 尾缀）。
    fn bundle_file_name(target_dir: &Path, generated_at: i64) -> String {
        let base = format!("{DIAGNOSTICS_FILE_PREFIX}-{generated_at}.json");
        if !target_dir.join(&base).exists() {
            return base;
        }
        format!(
            "{DIAGNOSTICS_FILE_PREFIX}-{generated_at}-{}.json",
            ulid::Ulid::new()
        )
    }
}

fn default_setting_value(key: &str) -> Value {
    match key {
        BACKUP_REMINDER_KEY => Value::Bool(true),
        _ => Value::Null,
    }
}

impl IpcBackend for DiagnosticsControlBackend {
    /// M3-05/D11：诊断包导出（脱敏 + 0 命中守门；日志/库摘要/健康/容量/任务 dump 整合）。
    fn export_diagnostics(
        &self,
        _request: &ExportDiagnosticsRequest,
        canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        // 空间护栏（ADR-003 决策 19，与备份同口径）：可写 + 可用空间 ≥ db+wal×1.2。
        self.space
            .ensure_writable(canonical_target_dir)
            .map_err(|message| {
                IpcError::invalid_value(format!("{message}（diagnostics_target_not_writable）"))
            })?;
        let required = required_space_bytes(file_len(&self.db_path()), file_len(&self.wal_path()));
        let available = self
            .space
            .available_bytes(canonical_target_dir)
            .map_err(|message| {
                IpcError::invalid_value(format!(
                    "目标空间探测失败（diagnostics_space_unknown）：{message}"
                ))
            })?;
        if available < required {
            return Err(IpcError::invalid_value(format!(
                "目标可用空间不足（diagnostics_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节（当前 db+wal 的 1.2 倍）"
            )));
        }

        let (bundle, _notes) = self.collect_bundle(self.clock.now_ms())?;
        let redactor = Redactor::new()
            .map_err(|error| IpcError::internal(format!("脱敏器初始化失败：{error}")))?;
        let redacted = redactor.redact_json(&bundle);
        let mut text = serde_json::to_string_pretty(&redacted)
            .map_err(|error| IpcError::internal(format!("诊断包序列化失败：{error}")))?;
        text.push('\n');
        // 写出前守门：脱敏后再扫描，任何命中都拒绝写出（D10 可判定口径）。
        if !redactor.is_clean(&text) {
            return Err(IpcError::internal(
                "诊断包脱敏后仍命中密钥模式，已拒绝写出（D10）",
            ));
        }

        let generated_at = self.clock.now_ms();
        let file_name = Self::bundle_file_name(canonical_target_dir, generated_at);
        let dest = canonical_target_dir.join(&file_name);
        std::fs::write(&dest, text.as_bytes()).map_err(|error| {
            IpcError::invalid_value(format!(
                "诊断包写出失败（diagnostics_write_failed）：{}：{error}",
                dest.display()
            ))
        })?;

        let sections: Vec<&str> = [
            "app",
            "host",
            "health",
            "security",
            "capacity",
            "store",
            "config",
            "logs",
            "task_dumps",
        ]
        .into_iter()
        .filter(|section| redacted.get(*section).is_some_and(|value| !value.is_null()))
        .collect();
        Ok(json!({
            "path": dest.to_string_lossy(),
            "file_name": file_name,
            "bytes": text.len(),
            "generated_at": generated_at,
            "sections": sections,
            "scanned_clean": true,
            "log_lines": self.logs.as_ref().map(|sink| sink.line_count()).unwrap_or(0),
            "task_dumps": self.task_dumps.as_ref().map(|source| source.task_dumps().len()).unwrap_or(0),
        }))
    }

    /// M3-05/D13：备份清单 + 未备份提醒（`reminder` 段；时钟注入 + 开关）。
    ///
    /// 容量段按本后端注入的 [`CapacityConfig`] 重算（与诊断包同一实例；阈值参数化）。
    fn backup_list(&self) -> Result<Value, IpcError> {
        let mut value = self.inner.backup_list()?;
        let reminder = self.reminder()?;
        let capacity = self
            .capacity
            .to_json(file_len(&self.db_path()), file_len(&self.wal_path()));
        if let Some(object) = value.as_object_mut() {
            object.insert("reminder".to_owned(), reminder);
            object.insert("capacity".to_owned(), capacity);
        }
        Ok(value)
    }

    /// M3-05：设置读取（已登记键白名单；缺省语义按键定义）。
    fn settings_get(&self, request: &SettingsGetRequest) -> Result<Value, IpcError> {
        Self::ensure_registered_key(&request.key)?;
        let value = self.setting_value(&request.key)?;
        Ok(json!({ "key": request.key, "value": value }))
    }

    /// M3-05：设置写入（经单写队列；值语义按键校验）。
    fn settings_set(&self, request: &SettingsSetRequest) -> Result<Value, IpcError> {
        Self::ensure_registered_key(&request.key)?;
        Self::validate_setting_value(&request.key, &request.value.0)?;
        let write = self.write_required()?;
        let key = request.key.clone();
        let value = request.value.0.clone();
        let text = serde_json::to_string(&value)
            .map_err(|error| IpcError::internal(format!("设置序列化失败：{error}")))?;
        let updated_at = self.clock.now_ms();
        self.call(async move {
            write
                .execute(StoreCommand::UpsertSetting {
                    key,
                    value: text,
                    updated_at,
                })
                .await
                .map(|_| ())
                .map_err(|error| IpcError::internal(format!("设置写入失败：{error}")))
        })?;
        Ok(json!({ "key": request.key, "value": value }))
    }

    // ===== 装饰器透传（未覆写的方法必须显式委派给内层，否则命中 trait 默认
    // `not_implemented`，令内层已实现命令在真实链路上不可达）。=====

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        self.inner.runtimes_list()
    }

    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
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

    fn backup_create(
        &self,
        request: &BackupCreateRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_create(request, canonical_target_dir)
    }

    fn backup_restore(
        &self,
        request: &BackupRestoreRequest,
        canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_restore(request, canonical_external_path)
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
}
