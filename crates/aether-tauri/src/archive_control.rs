//! 归档导出与保留清理命令后端（M5-05 / M5-04；设计 D13、ADR-021、ADR-018 命令登记）。
//!
//! 覆盖：
//! - `session_export`：单会话 JSONL（`sessions` 行 + `messages` 终稿 + 该会话 `events`
//!   按 `seq` 升序）+ manifest；外部目标目录（系统选择器 canonicalize）或默认
//!   `archives/`；空间护栏与 M3-04 同口径；脱敏 0 命中；原子写（临时文件 + rename）；
//! - `archive_export`：全库归档（`sessions`/`messages`/`events` 全量 + `runs` 摘要）
//!   + manifest；= 容量「一键归档」（UI-UX §8#16，锚点 `archive`）；
//! - 保留策略任务（[`RetentionTask`]）接线：`backup_list.retention` 段、诊断包
//!   `retention`/`export` 段（ADR-021 附录 A-3 指标）、后台调度器启动/停止；
//! - 审计（ADR-023 附录 A-1）：`session.exported` / `archive.exported`（actor=user）。
//!
//! 桥接口径与 `backup_control` 一致：async 存储操作 spawn 到核心运行时 +
//! `std::sync::mpsc` 同步等待（不阻塞运行时线程）。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aether_control::SharedClock;
use aether_security::Redactor;
use aether_store::archive::{export_full_archive, export_session_archive, ArchiveReport};
use aether_store::backup::required_space_bytes;
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, StoreError, WriteQueue};
use serde_json::{json, Value};

use crate::backup_control::{file_len, wal_path, NativeSpaceProbe, SpaceProbe};
use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    AppRestartRequest, ArchiveExportRequest, ArtifactAddRequest, ArtifactRemoveRequest,
    ArtifactsListRequest, BackupCreateRequest, BackupRestoreRequest, ExportDiagnosticsRequest,
    MessagesPageRequest, PermissionResolveRequest, PermissionsPendingRequest,
    ProviderCreateRequest, ProviderDeleteRequest, ProviderModelAddRequest,
    ProviderModelToggleRequest, ProviderToggleRequest, ProviderUpdateRequest, ProvidersListRequest,
    RunRetryRequest, RuntimeEnableRequest, RuntimeRetryRequest, SessionCreateRequest,
    SessionExportRequest, SessionIdRequest, SessionListRequest, SessionSendRequest,
    SettingsGetRequest, SettingsSetRequest, WorkspaceSetRequest,
};
use crate::ipc::error::IpcError;
use crate::ipc::path::ArtifactPath;
use crate::retention::{RetentionDeps, RetentionTask, RetentionThresholds};

/// 归档导出命令硬超时（大库流式写出预算）。
pub const ARCHIVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
/// 审计动作：全库归档导出（ADR-023 附录 A-1）。
pub const AUDIT_ACTION_ARCHIVE_EXPORTED: &str = "archive.exported";
/// 审计动作：单会话导出（ADR-023 附录 A-1）。
pub const AUDIT_ACTION_SESSION_EXPORTED: &str = "session.exported";

/// 最近一次导出指标（ADR-021 附录 A-3：`export.rows`/`export.bytes`/`export.sha256_ok`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportMetrics {
    pub at_ms: i64,
    pub source: String,
    pub rows: u64,
    pub bytes: u64,
    pub sha256_ok: bool,
    pub path: PathBuf,
}

/// 归档/保留命令后端（装饰器：其余命令透传内层）。
pub struct ArchiveControlBackend {
    inner: Arc<dyn IpcBackend>,
    data_dir: PathBuf,
    reads: Option<ReadPool>,
    write: Option<WriteQueue>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    space: Arc<dyn SpaceProbe>,
    clock: SharedClock,
    retention: Option<Arc<RetentionTask>>,
    export_metrics: Arc<Mutex<Option<ExportMetrics>>>,
}

impl ArchiveControlBackend {
    /// 组装（`reads` + `write` 均就绪时接线保留任务；降级启动为 `None`）。
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        data_dir: PathBuf,
        reads: Option<ReadPool>,
        write: Option<WriteQueue>,
        handle: tokio::runtime::Handle,
        clock: SharedClock,
    ) -> Self {
        let retention = match (reads.clone(), write.clone()) {
            (Some(reads), Some(write)) => Some(Arc::new(RetentionTask::new(RetentionDeps {
                data_dir: data_dir.clone(),
                reads,
                write,
                clock: clock.clone(),
                thresholds: RetentionThresholds::from_env(),
            }))),
            _ => None,
        };
        Self {
            inner,
            data_dir,
            reads,
            write,
            handle,
            timeout: ARCHIVE_COMMAND_TIMEOUT,
            space: Arc::new(NativeSpaceProbe),
            clock,
            retention,
            export_metrics: Arc::new(Mutex::new(None)),
        }
    }

    /// 注入空间探针（测试/故障注入：覆盖「空间不足/不可写」分支）。
    #[must_use]
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        self.space = space;
        self
    }

    /// 注入保留阈值（测试/演练参数化；容量两档 + 增长档）。
    #[must_use]
    pub fn with_retention_thresholds(self, thresholds: RetentionThresholds) -> Self {
        if let Some(retention) = &self.retention {
            retention.set_thresholds(thresholds);
        }
        self
    }

    /// 保留任务句柄（测试驱动 `tick`/`run_once`；生产由组合根启动）。
    pub fn retention_task(&self) -> Option<Arc<RetentionTask>> {
        self.retention.clone()
    }

    /// 启动保留下后台调度器（不阻塞主流程；`None` = 未接线）。
    pub fn spawn_retention(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.retention
            .as_ref()
            .map(|retention| retention.spawn(handle))
    }

    /// 默认归档目录（数据目录 `archives/`；ADR-021 实现要点 3）。
    pub fn default_archive_dir(&self) -> PathBuf {
        self.data_dir.join("archives")
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    fn reads_required(&self) -> Result<ReadPool, IpcError> {
        self.reads.clone().ok_or_else(|| {
            IpcError::core_not_ready("归档后端未接线：read 连接池不可用（启动失败或未完成）")
        })
    }

    fn lock_export_metrics(&self) -> MutexGuard<'_, Option<ExportMetrics>> {
        match self.export_metrics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
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
                "归档命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }

    /// 目标目录解析：外部（选择器 canonicalize）或默认 `archives/`；空间护栏
    /// （可写 + 可用 ≥ 当前 `db+wal` ×1.2，M3-04 同口径）。
    fn prepare_target(&self, canonical_target_dir: Option<&Path>) -> Result<PathBuf, IpcError> {
        let target = match canonical_target_dir {
            Some(dir) => dir.to_path_buf(),
            None => {
                let dir = self.default_archive_dir();
                std::fs::create_dir_all(&dir).map_err(|error| {
                    IpcError::internal(format!(
                        "创建默认归档目录失败（{}）：{error}",
                        dir.display()
                    ))
                })?;
                dir
            }
        };
        self.space.ensure_writable(&target).map_err(|message| {
            IpcError::invalid_value(format!("{message}（archive_target_not_writable）"))
        })?;
        let db = self.db_path();
        let required = required_space_bytes(file_len(&db), file_len(&wal_path(&db)));
        let available = self.space.available_bytes(&target).map_err(|message| {
            IpcError::invalid_value(format!(
                "目标空间探测失败（archive_space_unknown）：{message}"
            ))
        })?;
        if available < required {
            return Err(IpcError::invalid_value(format!(
                "目标可用空间不足（archive_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节（当前 db+wal 的 1.2 倍）；请选择空间充足的目录"
            )));
        }
        Ok(target)
    }

    /// 审计（写队列不可用（降级）时跳过；导出主路径不受影响）。
    fn write_audit(&self, action: &str, result: &str, resource: Option<&Path>, detail: &str) {
        let Some(write) = self.write.clone() else {
            return;
        };
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: None,
            runtime_id: None,
            actor: "user".to_owned(),
            action: action.to_owned(),
            resource: resource.map(|path| path.to_string_lossy().to_string()),
            detail: Some(detail.to_owned()),
            result: Some(result.to_owned()),
            ts: self.clock.now_ms(),
        };
        if let Err(error) = self.call(async move {
            write
                .execute(StoreCommand::InsertAudit { record })
                .await
                .map(|_| ())
                .map_err(|error| IpcError::internal(format!("导出审计写入失败：{error}")))
        }) {
            tracing::warn!(error = %error, "导出审计写入失败（不阻断导出结果）");
        }
    }

    /// 记录最近导出指标（诊断包 `export` 段；ADR-021 附录 A-3）。
    fn record_export_metrics(&self, report: &ArchiveReport) {
        let mut slot = self.lock_export_metrics();
        *slot = Some(ExportMetrics {
            at_ms: self.clock.now_ms(),
            source: report.source.clone(),
            rows: report.rows,
            bytes: report.bytes,
            sha256_ok: true,
            path: report.path.clone(),
        });
    }

    /// 导出响应（`{ "export": ArchiveReport }`）。
    fn export_response(report: &ArchiveReport) -> Value {
        json!({ "export": report })
    }
}

impl IpcBackend for ArchiveControlBackend {
    /// M5-05 `session_export`：单会话 JSONL + manifest（ADR-018 附录 A-1）。
    fn session_export(
        &self,
        request: &SessionExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let target = match self.prepare_target(canonical_target_dir) {
            Ok(target) => target,
            Err(error) => {
                self.write_audit(
                    AUDIT_ACTION_SESSION_EXPORTED,
                    "failed",
                    None,
                    &error.to_string(),
                );
                return Err(error);
            }
        };
        let session_id = request.session_id.clone();
        let created_at = self.clock.now_ms();
        let target_for_task = target.clone();
        let result = self.call(async move {
            reads
                .with_connection(move |connection| {
                    let redactor = Redactor::new().map_err(|error| StoreError::Internal {
                        reason: format!("脱敏器初始化失败：{error}"),
                    })?;
                    let sanitize = |value: &Value| -> Result<String, StoreError> {
                        sanitize_archive_line(&redactor, value)
                    };
                    export_session_archive(
                        connection,
                        &target_for_task,
                        &session_id,
                        created_at,
                        &sanitize,
                    )
                })
                .await
                .map_err(|error| {
                    IpcError::internal(format!("会话导出失败（session_export）：{error}"))
                })
        })?;
        match result {
            None => {
                let message = format!("会话不存在（session_not_found）：{}", request.session_id);
                self.write_audit(AUDIT_ACTION_SESSION_EXPORTED, "failed", None, &message);
                Err(IpcError::invalid_value(message))
            }
            Some(report) => {
                self.record_export_metrics(&report);
                self.write_audit(
                    AUDIT_ACTION_SESSION_EXPORTED,
                    "success",
                    Some(&report.path),
                    &format!(
                        "rows={}; bytes={}; sha256={}; session_id={}",
                        report.rows, report.bytes, report.sha256, request.session_id
                    ),
                );
                Ok(Self::export_response(&report))
            }
        }
    }

    /// M5-05 `archive_export`：全库归档（容量「一键归档」，UI-UX §8#16）。
    fn archive_export(
        &self,
        _request: &ArchiveExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?;
        let target = match self.prepare_target(canonical_target_dir) {
            Ok(target) => target,
            Err(error) => {
                self.write_audit(
                    AUDIT_ACTION_ARCHIVE_EXPORTED,
                    "failed",
                    None,
                    &error.to_string(),
                );
                return Err(error);
            }
        };
        let created_at = self.clock.now_ms();
        let target_for_task = target.clone();
        let result = self.call(async move {
            reads
                .with_connection(move |connection| {
                    let redactor = Redactor::new().map_err(|error| StoreError::Internal {
                        reason: format!("脱敏器初始化失败：{error}"),
                    })?;
                    let sanitize = |value: &Value| -> Result<String, StoreError> {
                        sanitize_archive_line(&redactor, value)
                    };
                    export_full_archive(connection, &target_for_task, created_at, &sanitize)
                })
                .await
                .map_err(|error| {
                    IpcError::internal(format!("全库归档失败（archive_export）：{error}"))
                })
        });
        match result {
            Ok(report) => {
                self.record_export_metrics(&report);
                self.write_audit(
                    AUDIT_ACTION_ARCHIVE_EXPORTED,
                    "success",
                    Some(&report.path),
                    &format!(
                        "rows={}; bytes={}; sha256={}",
                        report.rows, report.bytes, report.sha256
                    ),
                );
                Ok(Self::export_response(&report))
            }
            Err(error) => {
                self.write_audit(
                    AUDIT_ACTION_ARCHIVE_EXPORTED,
                    "failed",
                    None,
                    &error.to_string(),
                );
                Err(error)
            }
        }
    }

    /// 保留任务状态（内部接线，非 IPC 命令；诊断包 `retention` 段）。
    fn retention_status(&self) -> Result<Option<Value>, IpcError> {
        match &self.retention {
            Some(retention) => {
                let retention = Arc::clone(retention);
                self.call(async move { retention.status().await }).map(Some)
            }
            None => Ok(None),
        }
    }

    /// `backup_list` 增量：保留策略段（`retention`；UI 状态与阈值联动展示）。
    fn backup_list(&self) -> Result<Value, IpcError> {
        let mut value = self.inner.backup_list()?;
        if let Some(retention) = self.retention_status()? {
            if let Some(object) = value.as_object_mut() {
                object.insert("retention".to_owned(), retention);
            }
        }
        Ok(value)
    }

    /// 诊断包 `export` 段投影（ADR-021 附录 A-3 指标；内部接线）。
    fn export_metrics_status(&self) -> Result<Option<Value>, IpcError> {
        let slot = self.lock_export_metrics();
        Ok(slot.as_ref().map(|metrics| {
            json!({
                "at": metrics.at_ms,
                "source": metrics.source,
                "rows": metrics.rows,
                "bytes": metrics.bytes,
                "sha256_ok": metrics.sha256_ok,
                "path": metrics.path.to_string_lossy(),
            })
        }))
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

    fn settings_get(&self, request: &SettingsGetRequest) -> Result<Value, IpcError> {
        self.inner.settings_get(request)
    }

    fn settings_set(&self, request: &SettingsSetRequest) -> Result<Value, IpcError> {
        self.inner.settings_set(request)
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

    fn backup_auto_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.backup_auto_status()
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

    fn artifacts_list(&self, request: &ArtifactsListRequest) -> Result<Value, IpcError> {
        self.inner.artifacts_list(request)
    }

    fn artifact_add(
        &self,
        request: &ArtifactAddRequest,
        resolved: &ArtifactPath,
    ) -> Result<Value, IpcError> {
        self.inner.artifact_add(request, resolved)
    }

    fn artifact_remove(&self, request: &ArtifactRemoveRequest) -> Result<Value, IpcError> {
        self.inner.artifact_remove(request)
    }

    fn providers_list(&self, request: &ProvidersListRequest) -> Result<Value, IpcError> {
        self.inner.providers_list(request)
    }

    fn provider_create(&self, request: &ProviderCreateRequest) -> Result<Value, IpcError> {
        self.inner.provider_create(request)
    }

    fn provider_update(&self, request: &ProviderUpdateRequest) -> Result<Value, IpcError> {
        self.inner.provider_update(request)
    }

    fn provider_delete(&self, request: &ProviderDeleteRequest) -> Result<Value, IpcError> {
        self.inner.provider_delete(request)
    }

    fn provider_toggle(&self, request: &ProviderToggleRequest) -> Result<Value, IpcError> {
        self.inner.provider_toggle(request)
    }

    fn provider_model_add(&self, request: &ProviderModelAddRequest) -> Result<Value, IpcError> {
        self.inner.provider_model_add(request)
    }

    fn provider_model_toggle(
        &self,
        request: &ProviderModelToggleRequest,
    ) -> Result<Value, IpcError> {
        self.inner.provider_model_toggle(request)
    }

    fn export_diagnostics(
        &self,
        request: &ExportDiagnosticsRequest,
        canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.export_diagnostics(request, canonical_target_dir)
    }
}

/// 归档行脱敏 + 0 命中守门（D10；与诊断包同一实现）。
fn sanitize_archive_line(redactor: &Redactor, value: &Value) -> Result<String, StoreError> {
    let redacted = redactor.redact_json(value);
    let line = serde_json::to_string(&redacted).map_err(|error| StoreError::Internal {
        reason: format!("归档行序列化失败：{error}"),
    })?;
    if !redactor.is_clean(&line) {
        return Err(StoreError::Internal {
            reason: "归档行脱敏后仍命中密钥模式，已拒绝写出（D10）".to_owned(),
        });
    }
    Ok(line)
}
