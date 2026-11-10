//! 导入/导出命令后端（M10-04 / ADR-034；设计 D3/D4/D10/D13）。
//!
//! 职责：
//! - `session_import` / `archive_import`：同步做路径/大小/首行预检，创建持久化作业，
//!   异步流式校验；`dry_run` 在当前库快照上生成 staging 预览，真实导入在重启后由
//!   启动序列执行 staging + 原子切换（`aether-store::import::boot_apply_pending_import`）。
//! - `import_status` / `import_cancel`：轮询/取消（staging/validating/applying）。
//! - 维护窗口：真实导入活动期间，命令层拒绝变更类命令（`storage_backpressure`），
//!   只读/导入控制/生命周期命令保留；`backup_restore` 与导入互斥。
//! - 审计：`import.started/completed/failed/cancelled` + `export.started/completed/failed`
//!   （ADR-034 D-034-5；`dry_run` 不写审计）。
//!
//! 本后端为**最外层**装饰器：其余命令显式透传内层，避免 trait 默认实现吞掉已接线命令。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aether_control::SharedClock;
use aether_store::import::{
    self, ImportBootReport, ImportFailure, ImportFailureInfo, ImportJob, ImportJobStatus, ImportMode,
    ImportProgress, ARCHIVE_IMPORT_SOURCES, AUDIT_ACTION_IMPORT_CANCELLED,
    AUDIT_ACTION_IMPORT_FAILED, AUDIT_ACTION_IMPORT_STARTED, SESSION_IMPORT_SOURCES,
};
use aether_store::{AuditLogRecord, StoreCommand, WriteQueue};
use serde_json::{json, Value};

use crate::backup_control::{file_len, wal_path, NativeSpaceProbe, SpaceProbe};
use crate::ipc::backend::IpcBackend;
use crate::ipc::business::BusinessCode;
use crate::ipc::dto::{
    AppRestartRequest, ArchiveExportRequest, ArtifactAddRequest, ArtifactRemoveRequest,
    ArtifactsListRequest, AuditExportRequest, AuditQueryRequest, BackupCreateRequest,
    BackupRestoreRequest, ExportDiagnosticsRequest,
    ImportCancelRequest, ImportRequest, ImportStatusRequest, MessagesPageRequest,
    PermissionResolveRequest, PermissionsPendingRequest, PresetAddRequest, PresetDeleteRequest,
    PresetUpdateRequest, PresetsListRequest, ProviderCreateRequest, ProviderDeleteRequest,
    ProviderModelAddRequest, ProviderModelToggleRequest, ProviderModelsFetchRequest,
    ProviderTestRequest, ProviderToggleRequest, ProviderUpdateRequest, ProvidersListRequest,
    RuleAddRequest, RuleDeleteRequest, RuleToggleRequest, RuleUpdateRequest,
    RunRetryRequest, RuntimeDisableRequest, RuntimeEnableRequest,
    RuntimeRetryRequest, SessionCreateRequest, SessionDeleteRequest, SessionExportRequest,
    SessionIdRequest, SessionListRequest, SessionPermissionModeSetRequest, SessionRenameRequest,
    SessionSearchRequest, SessionSendRequest, SessionWorkspaceAssignRequest, SettingsGetRequest,
    SettingsSetRequest,
    SubagentCancelRequest, SubagentListRequest, SubagentResolveRequest, SubagentResultGetRequest,
    SubagentSpawnRequest, WorkspaceCreateRequest, WorkspaceDeleteRequest, WorkspaceSetRequest,
    WorkspaceUpdateRequest, WorkspacesListRequest, WorkflowCreateRequest, WorkflowDeleteRequest,
    WorkflowIdRequest, WorkflowImportRequest, WorkflowListRequest, WorkflowNodeRetryRequest,
    WorkflowRunCancelRequest, WorkflowRunGetRequest, WorkflowRunListRequest, WorkflowRunRequest,
    WorkflowUpdateRequest,
};
use crate::ipc::error::IpcError;
use crate::ipc::path::ArtifactPath;

/// 导出审计动作（ADR-034 D-034-5 增行；与 M5-05 既有 `*.exported` 并存）。
pub const AUDIT_ACTION_EXPORT_STARTED: &str = "export.started";
/// 导出完成。
pub const AUDIT_ACTION_EXPORT_COMPLETED: &str = "export.completed";
/// 导出失败。
pub const AUDIT_ACTION_EXPORT_FAILED: &str = "export.failed";

macro_rules! forward_request {
    ($($name:ident : $ty:ty),* $(,)?) => {
        $(fn $name(&self, request: &$ty) -> Result<Value, IpcError> { self.inner.$name(request) })*
    };
}

macro_rules! forward_none {
    ($($name:ident),* $(,)?) => {
        $(fn $name(&self) -> Result<Value, IpcError> { self.inner.$name() })*
    };
}

#[derive(Clone)]
struct ActiveJob {
    job: Arc<Mutex<ImportJob>>,
    cancel: Arc<AtomicBool>,
}

/// 导入/导出装饰器（最外层）。
#[derive(Clone)]
pub struct ImportControlBackend {
    inner: Arc<dyn IpcBackend>,
    data_dir: PathBuf,
    write: WriteQueue,
    handle: tokio::runtime::Handle,
    clock: SharedClock,
    space: Arc<dyn SpaceProbe>,
    active: Arc<Mutex<Option<ActiveJob>>>,
}

impl ImportControlBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        data_dir: PathBuf,
        write: WriteQueue,
        handle: tokio::runtime::Handle,
        clock: SharedClock,
    ) -> Self {
        Self {
            inner,
            data_dir,
            write,
            handle,
            clock,
            space: Arc::new(NativeSpaceProbe),
            active: Arc::new(Mutex::new(None)),
        }
    }

    /// 注入空间探针（测试/故障注入）。
    #[must_use]
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        self.space = space;
        self
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("aether.db")
    }

    fn lock_active(&self) -> MutexGuard<'_, Option<ActiveJob>> {
        match self.active.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn audit(
        &self,
        action: &'static str,
        result: &'static str,
        resource: Option<String>,
        detail: Value,
    ) {
        let write = self.write.clone();
        let at_ms = self.clock.now_ms();
        let detail = detail.to_string();
        self.handle.spawn(async move {
            let record = AuditLogRecord {
                id: ulid::Ulid::new().to_string(),
                session_id: None,
                runtime_id: None,
                actor: "user".to_owned(),
                action: action.to_owned(),
                resource,
                detail: Some(detail),
                result: Some(result.to_owned()),
                ts: at_ms,
            };
            let _ = write.execute(StoreCommand::InsertAudit { record }).await;
        });
    }

    fn import_in_progress(&self) -> bool {
        if import::pending_exists(&self.data_dir) {
            return true;
        }
        let active = self.lock_active();
        active.as_ref().is_some_and(|active| {
            active
                .job
                .lock()
                .map(|job| job.status.is_active())
                .unwrap_or(false)
        })
    }

    fn finish_active(&self, job_id: &str) {
        let mut slot = self.lock_active();
        let matches = slot
            .as_ref()
            .and_then(|active| active.job.lock().ok().map(|job| job.job_id == job_id))
            .unwrap_or(false);
        if matches {
            *slot = None;
        }
    }

    fn ensure_space(&self, source_bytes: u64) -> Result<(), IpcError> {
        let db = self.db_path();
        let required = aether_store::backup::required_space_bytes(source_bytes, 0)
            .saturating_add(aether_store::backup::required_space_bytes(
                file_len(&db),
                file_len(&wal_path(&db)),
            ));
        let available = self
            .space
            .available_bytes(&self.data_dir)
            .map_err(|message| IpcError::internal(format!("导入空间探测失败：{message}")))?;
        if available < required {
            return Err(map_failure(ImportFailure::too_large(format!(
                "数据目录可用空间不足（import_space_insufficient）：可用 {available} 字节 < 需求 {required} 字节"
            ))));
        }
        Ok(())
    }

    fn start_import(
        &self,
        kind: &'static str,
        request: &ImportRequest,
        source_path: &Path,
        expected_sources: &'static [&'static str],
    ) -> Result<Value, IpcError> {
        if aether_store::backup::read_restore_journal(&self.db_path())
            .ok()
            .flatten()
            .is_some()
        {
            return Err(IpcError::invalid_value(
                "恢复进行中：导入与 backup_restore 互斥；请等待恢复完成或重启后再试",
            )
            .with_business_code(BusinessCode::ImportJobNotActive));
        }
        if self.import_in_progress() {
            return Err(IpcError::invalid_value(
                "已有导入作业进行中（同一时间只允许 1 个导入）；请等待完成或取消",
            )
            .with_business_code(BusinessCode::ImportConflict));
        }
        let manifest =
            import::precheck_source(source_path, expected_sources).map_err(map_failure)?;
        let source_bytes = std::fs::metadata(source_path)
            .map(|metadata| metadata.len())
            .map_err(|error| IpcError::internal(format!("导入源读取失败：{error}")))?;
        self.ensure_space(source_bytes)?;

        let now = self.clock.now_ms();
        let job_id = import::new_ulid();
        let dry_run = request.dry_run_value();
        let mode = request.mode_value();
        let job = ImportJob {
            v: import::IMPORT_JOB_VERSION,
            job_id: job_id.clone(),
            kind: kind.to_owned(),
            status: ImportJobStatus::Staging,
            mode,
            dry_run,
            source_path: source_path.to_string_lossy().to_string(),
            source_bytes,
            source_sha256: Some(manifest.sha256),
            progress: ImportProgress {
                rows: 0,
                bytes: 0,
                total_bytes: Some(source_bytes),
            },
            summary: None,
            warnings: Vec::new(),
            warnings_truncated: false,
            error: None,
            created_at: now,
            updated_at: now,
        };
        import::write_job(&self.data_dir, &job).map_err(map_failure)?;
        if !dry_run {
            self.audit(
                AUDIT_ACTION_IMPORT_STARTED,
                "started",
                Some(job_id.clone()),
                json!({
                    "source": source_path.to_string_lossy(),
                    "kind": kind,
                    "mode": mode.as_str(),
                }),
            );
        }
        let shared = Arc::new(Mutex::new(job));
        let cancel = Arc::new(AtomicBool::new(false));
        *self.lock_active() = Some(ActiveJob {
            job: Arc::clone(&shared),
            cancel: Arc::clone(&cancel),
        });
        let this = self.clone();
        let source = source_path.to_path_buf();
        self.handle.spawn(async move {
            this.run_job(shared, cancel, source, expected_sources)
                .await;
        });
        Ok(json!({"job_id": job_id, "status": "staging"}))
    }

    async fn run_job(
        &self,
        shared: Arc<Mutex<ImportJob>>,
        cancel: Arc<AtomicBool>,
        source_path: PathBuf,
        expected_sources: &'static [&'static str],
    ) {
        let (job_id, dry_run) = match shared.lock() {
            Ok(job) => (job.job_id.clone(), job.dry_run),
            Err(_) => return,
        };
        self.update_job(&shared, ImportJobStatus::Validating, |_job| {});

        let can = Arc::clone(&cancel);
        let shared_for_scan = Arc::clone(&shared);
        let data_dir = self.data_dir.clone();
        let source_for_scan = source_path.clone();
        let validation = tokio::task::spawn_blocking(move || {
            let mut last_persist = Instant::now();
            let mut progress = |bytes: u64, rows: u64| {
                if can.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut job) = shared_for_scan.lock() else {
                    return;
                };
                if job.status == ImportJobStatus::Cancelled {
                    return;
                }
                job.progress.bytes = bytes;
                job.progress.rows = rows;
                if last_persist.elapsed() >= Duration::from_secs(1) {
                    last_persist = Instant::now();
                    job.updated_at = import::current_time_ms();
                    let _ = import::write_job(&data_dir, &job);
                }
            };
            import::validate_source(
                &source_for_scan,
                expected_sources,
                &mut progress,
                &|| can.load(Ordering::SeqCst),
            )
        })
        .await;

        let validation = match validation {
            Ok(Ok(validation)) => validation,
            Ok(Err(failure)) => {
                if failure.kind == aether_store::import::ImportFailureKind::Cancelled
                    || cancel.load(Ordering::SeqCst)
                {
                    self.finish_cancelled(&shared, &job_id, dry_run).await;
                } else {
                    self.finish_failed(&shared, &job_id, failure, dry_run).await;
                }
                return;
            }
            Err(error) => {
                let failure =
                    ImportFailure::internal(format!("导入校验任务异常退出：{error}"));
                self.finish_failed(&shared, &job_id, failure, dry_run).await;
                return;
            }
        };

        if cancel.load(Ordering::SeqCst) {
            self.finish_cancelled(&shared, &job_id, dry_run).await;
            return;
        }

        if dry_run {
            let mode = shared
                .lock()
                .map(|job| job.mode)
                .unwrap_or(ImportMode::Remap);
            self.update_job(&shared, ImportJobStatus::Applying, |job| {
                job.summary = Some(validation.summary);
                job.warnings = validation.warnings.clone();
                job.warnings_truncated = validation.warnings_truncated;
                job.progress.rows = validation.rows;
                job.progress.bytes = validation.bytes;
            });
            let can = Arc::clone(&cancel);
            let data_dir = self.data_dir.clone();
            let db_path = self.db_path();
            let job_for_work = job_id.clone();
            let source = source_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                let work_db = import::work_db_path(&data_dir, &job_for_work);
                let outcome = import::create_staging_snapshot(Some(&db_path), &work_db)
                    .and_then(|()| {
                        import::apply_source_to_staging(
                            &source,
                            &work_db,
                            expected_sources,
                            mode,
                            import::current_time_ms(),
                            &mut |_bytes, _rows| {},
                            &|| can.load(Ordering::SeqCst),
                        )
                    })
                    .and_then(|report| {
                        import::verify_staging(&work_db)?;
                        Ok(report)
                    });
                let _ = import::cleanup_staging(&data_dir, &job_for_work);
                outcome
            })
            .await;
            match result {
                Ok(Ok(report)) => {
                    self.update_job(&shared, ImportJobStatus::Completed, |job| {
                        job.summary = Some(report.summary);
                        job.progress.rows = report.rows;
                        job.progress.bytes = report.bytes;
                        job.progress.total_bytes = Some(report.bytes);
                        job.warnings = report.warnings;
                        job.warnings_truncated = report.warnings_truncated;
                        job.error = None;
                    });
                    self.finish_active(&job_id);
                }
                Ok(Err(failure)) => {
                    if failure.kind == aether_store::import::ImportFailureKind::Cancelled
                        || cancel.load(Ordering::SeqCst)
                    {
                        self.finish_cancelled(&shared, &job_id, true).await;
                    } else {
                        self.finish_failed(&shared, &job_id, failure, true).await;
                    }
                }
                Err(error) => {
                    let failure =
                        ImportFailure::internal(format!("导入预览任务异常退出：{error}"));
                    self.finish_failed(&shared, &job_id, failure, true).await;
                }
            }
            return;
        }

        // 真实导入：登记待重启；切换在下次启动的无写者窗口执行。
        if cancel.load(Ordering::SeqCst) {
            let _ = import::remove_pending(&self.data_dir);
            self.finish_cancelled(&shared, &job_id, false).await;
            return;
        }
        if let Err(error) = import::write_pending(&self.data_dir, &job_id, self.clock.now_ms()) {
            self.finish_failed(&shared, &job_id, error, false).await;
            return;
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = import::remove_pending(&self.data_dir);
            self.finish_cancelled(&shared, &job_id, false).await;
            return;
        }
        self.update_job(&shared, ImportJobStatus::AwaitingRestart, |job| {
            job.summary = Some(validation.summary);
            job.warnings = validation.warnings;
            job.warnings_truncated = validation.warnings_truncated;
            job.progress.rows = validation.rows;
            job.progress.bytes = validation.bytes;
            job.progress.total_bytes = Some(validation.bytes);
            job.error = None;
        });
    }

    fn update_job(
        &self,
        shared: &Arc<Mutex<ImportJob>>,
        status: ImportJobStatus,
        mutate: impl FnOnce(&mut ImportJob),
    ) {
        let Ok(mut job) = shared.lock() else {
            return;
        };
        if job.status == ImportJobStatus::Cancelled && status != ImportJobStatus::Cancelled {
            return;
        }
        job.status = status;
        job.updated_at = self.clock.now_ms();
        mutate(&mut job);
        let _ = import::write_job(&self.data_dir, &job);
    }

    async fn finish_failed(
        &self,
        shared: &Arc<Mutex<ImportJob>>,
        job_id: &str,
        failure: ImportFailure,
        dry_run: bool,
    ) {
        {
            let Ok(mut job) = shared.lock() else {
                return;
            };
            if job.status == ImportJobStatus::Cancelled {
                drop(job);
                self.finish_active(job_id);
                return;
            }
            job.status = ImportJobStatus::Failed;
            job.error = Some(ImportFailureInfo {
                code: failure.business_code().to_owned(),
                message: failure.message.clone(),
                details: failure.details.clone(),
            });
            job.updated_at = self.clock.now_ms();
            let _ = import::write_job(&self.data_dir, &job);
        }
        if !dry_run {
            self.audit(
                AUDIT_ACTION_IMPORT_FAILED,
                "failed",
                Some(job_id.to_owned()),
                json!({"error": failure.message}),
            );
        }
        let _ = import::cleanup_staging(&self.data_dir, job_id);
        self.finish_active(job_id);
    }

    async fn finish_cancelled(
        &self,
        shared: &Arc<Mutex<ImportJob>>,
        job_id: &str,
        dry_run: bool,
    ) {
        {
            let Ok(mut job) = shared.lock() else {
                return;
            };
            job.status = ImportJobStatus::Cancelled;
            job.updated_at = self.clock.now_ms();
            let _ = import::write_job(&self.data_dir, &job);
        }
        if !dry_run {
            self.audit(
                AUDIT_ACTION_IMPORT_CANCELLED,
                "cancelled",
                Some(job_id.to_owned()),
                json!({"status": "cancelled"}),
            );
        }
        let _ = import::cleanup_staging(&self.data_dir, job_id);
        self.finish_active(job_id);
    }

    fn with_export_audit(
        &self,
        action: &'static str,
        resource: Option<String>,
        run: impl FnOnce() -> Result<Value, IpcError>,
    ) -> Result<Value, IpcError> {
        self.audit(AUDIT_ACTION_EXPORT_STARTED, "started", resource.clone(), json!({"action": action}));
        match run() {
            Ok(value) => {
                let detail = value
                    .get("export")
                    .map(|export| {
                        json!({
                            "rows": export.get("rows").cloned().unwrap_or(Value::Null),
                            "bytes": export.get("bytes").cloned().unwrap_or(Value::Null),
                            "sha256": export.get("sha256").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .unwrap_or_else(|| json!({}));
                self.audit(AUDIT_ACTION_EXPORT_COMPLETED, "success", resource, detail);
                Ok(value)
            }
            Err(error) => {
                self.audit(
                    AUDIT_ACTION_EXPORT_FAILED,
                    "failed",
                    resource,
                    json!({"error": error.to_string()}),
                );
                Err(error)
            }
        }
    }
}

/// 启动序列入口：应用待重启导入（`aether-store` 实现；app 注入空间探针）。
pub fn boot_apply_pending_import<F>(
    data_dir: &Path,
    at_ms: i64,
    available_bytes: F,
) -> Result<Option<ImportBootReport>, ImportFailure>
where
    F: Fn(&Path) -> Result<u64, String>,
{
    aether_store::import::boot_apply_pending_import(data_dir, at_ms, available_bytes)
}

/// `ImportFailure` → 结构化 IPC 错误（业务码不扩张 32 枚举）。
fn map_failure(failure: ImportFailure) -> IpcError {
    let code = match failure.kind {
        aether_store::import::ImportFailureKind::InvalidFormat => BusinessCode::ImportInvalidFormat,
        aether_store::import::ImportFailureKind::ChecksumMismatch => {
            BusinessCode::ImportChecksumMismatch
        }
        aether_store::import::ImportFailureKind::Conflict => BusinessCode::ImportConflict,
        aether_store::import::ImportFailureKind::TooLarge => BusinessCode::ImportTooLarge,
        aether_store::import::ImportFailureKind::Cancelled => BusinessCode::ImportCancelled,
        aether_store::import::ImportFailureKind::JobNotFound => BusinessCode::ImportJobNotFound,
        aether_store::import::ImportFailureKind::JobNotActive => BusinessCode::ImportJobNotActive,
        aether_store::import::ImportFailureKind::Internal => {
            return IpcError::internal(failure.message);
        }
    };
    let base = match failure.kind {
        aether_store::import::ImportFailureKind::InvalidFormat => {
            IpcError::invalid_format("source_path", failure.message)
        }
        aether_store::import::ImportFailureKind::TooLarge => {
            IpcError::too_large("source_path", failure.message)
        }
        _ => IpcError::invalid_value(failure.message),
    };
    let base = base.with_business_code(code);
    match failure.details {
        Some(details) => base.with_details(details),
        None => base,
    }
}

impl IpcBackend for ImportControlBackend {
    fn runtimes_list(&self) -> Result<Value, IpcError> {
        self.inner.runtimes_list()
    }

    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    forward_request!(
        session_list: SessionListRequest,
        session_search: SessionSearchRequest,
        session_rename: SessionRenameRequest,
        session_delete: SessionDeleteRequest,
        session_create: SessionCreateRequest,
        session_send: SessionSendRequest,
        session_interrupt: SessionIdRequest,
        session_pause: SessionIdRequest,
        session_resume: SessionIdRequest,
        session_dispose: SessionIdRequest,
        messages_page: MessagesPageRequest,
        permissions_pending: PermissionsPendingRequest,
        permission_resolve: PermissionResolveRequest,
        session_permission_mode_set: SessionPermissionModeSetRequest,
        rule_add: RuleAddRequest,
        rule_update: RuleUpdateRequest,
        rule_delete: RuleDeleteRequest,
        rule_toggle: RuleToggleRequest,
        settings_get: SettingsGetRequest,
        settings_set: SettingsSetRequest,
        run_retry: RunRetryRequest,
        runtime_retry: RuntimeRetryRequest,
        runtime_enable: RuntimeEnableRequest,
        runtime_disable: RuntimeDisableRequest,
        workspace_update: WorkspaceUpdateRequest,
        workspace_delete: WorkspaceDeleteRequest,
        artifacts_list: ArtifactsListRequest,
        artifact_remove: ArtifactRemoveRequest,
        providers_list: ProvidersListRequest,
        provider_create: ProviderCreateRequest,
        provider_update: ProviderUpdateRequest,
        provider_delete: ProviderDeleteRequest,
        provider_toggle: ProviderToggleRequest,
        provider_model_add: ProviderModelAddRequest,
        provider_model_toggle: ProviderModelToggleRequest,
        provider_test: ProviderTestRequest,
        provider_models_fetch: ProviderModelsFetchRequest,
        presets_list: PresetsListRequest,
        preset_add: PresetAddRequest,
        preset_update: PresetUpdateRequest,
        preset_delete: PresetDeleteRequest,
        audit_query: AuditQueryRequest,
        workspaces_list: WorkspacesListRequest,
        workflow_create: WorkflowCreateRequest,
        workflow_list: WorkflowListRequest,
        workflow_get: WorkflowIdRequest,
        workflow_update: WorkflowUpdateRequest,
        workflow_delete: WorkflowDeleteRequest,
        workflow_import: WorkflowImportRequest,
        workflow_run: WorkflowRunRequest,
        workflow_run_get: WorkflowRunGetRequest,
        workflow_run_list: WorkflowRunListRequest,
        workflow_run_cancel: WorkflowRunCancelRequest,
        workflow_node_retry: WorkflowNodeRetryRequest,
        subagent_spawn: SubagentSpawnRequest,
        subagent_list: SubagentListRequest,
        subagent_cancel: SubagentCancelRequest,
        subagent_resolve: SubagentResolveRequest,
        subagent_result_get: SubagentResultGetRequest,
    );

    forward_none!(rules_list, backup_list);

    fn app_restart(&self, request: &AppRestartRequest) -> Result<Value, IpcError> {
        self.inner.app_restart(request)
    }

    fn backup_auto_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.backup_auto_status()
    }

    fn retention_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.retention_status()
    }

    fn export_metrics_status(&self) -> Result<Option<Value>, IpcError> {
        self.inner.export_metrics_status()
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
        if self.import_in_progress() {
            return Err(IpcError::invalid_value(
                "导入进行中：恢复与导入互斥；请等待导入完成/取消或重启完成导入",
            )
            .with_business_code(BusinessCode::ImportJobNotActive));
        }
        self.inner.backup_restore(request, canonical_external_path)
    }

    fn workspace_set(
        &self,
        request: &WorkspaceSetRequest,
        canonical_root_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.workspace_set(request, canonical_root_path)
    }

    fn workspace_create(
        &self,
        request: &WorkspaceCreateRequest,
        canonical_root_path: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.workspace_create(request, canonical_root_path)
    }

    fn session_workspace_assign(
        &self,
        request: &SessionWorkspaceAssignRequest,
    ) -> Result<Value, IpcError> {
        self.inner.session_workspace_assign(request)
    }

    fn artifact_add(
        &self,
        request: &ArtifactAddRequest,
        resolved: &ArtifactPath,
    ) -> Result<Value, IpcError> {
        self.inner.artifact_add(request, resolved)
    }

    fn export_diagnostics(
        &self,
        request: &ExportDiagnosticsRequest,
        canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        self.inner.export_diagnostics(request, canonical_target_dir)
    }

    fn session_export(
        &self,
        request: &SessionExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.with_export_audit(
            "session_export",
            Some(request.session_id.clone()),
            || self.inner.session_export(request, canonical_target_dir),
        )
    }

    fn archive_export(
        &self,
        request: &ArchiveExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.with_export_audit("archive_export", None, || {
            self.inner.archive_export(request, canonical_target_dir)
        })
    }

    fn audit_export(
        &self,
        request: &AuditExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.with_export_audit("audit_export", Some("audit".to_owned()), || {
            self.inner.audit_export(request, canonical_target_dir)
        })
    }

    fn session_import(
        &self,
        request: &ImportRequest,
        canonical_source_path: &Path,
    ) -> Result<Value, IpcError> {
        self.start_import("session", request, canonical_source_path, &SESSION_IMPORT_SOURCES)
    }

    fn archive_import(
        &self,
        request: &ImportRequest,
        canonical_source_path: &Path,
    ) -> Result<Value, IpcError> {
        self.start_import("archive", request, canonical_source_path, &ARCHIVE_IMPORT_SOURCES)
    }

    fn import_status(&self, request: &ImportStatusRequest) -> Result<Value, IpcError> {
        let include_warnings = request.include_warnings.unwrap_or(false);
        let active = self.lock_active().clone();
        if let Some(active) = active {
            if let Ok(job) = active.job.lock() {
                if job.job_id == request.job_id {
                    return Ok(import::job_status_value(&job, include_warnings));
                }
            }
        }
        let job = import::read_job(&self.data_dir, &request.job_id).map_err(map_failure)?;
        match job {
            Some(job) => Ok(import::job_status_value(&job, include_warnings)),
            None => Err(map_failure(ImportFailure::job_not_found(&request.job_id))),
        }
    }

    fn import_cancel(&self, request: &ImportCancelRequest) -> Result<Value, IpcError> {
        let active = self.lock_active().clone();
        if let Some(active) = active {
            let Ok(mut job) = active.job.lock() else {
                return Err(IpcError::internal("导入作业状态锁不可用"));
            };
            if job.job_id != request.job_id {
                drop(job);
            } else {
                if !matches!(
                    job.status,
                    ImportJobStatus::Staging
                        | ImportJobStatus::Validating
                        | ImportJobStatus::Applying
                ) {
                    return Err(map_failure(ImportFailure::job_not_active(
                        &request.job_id,
                        job.status,
                    )));
                }
                active.cancel.store(true, Ordering::SeqCst);
                job.status = ImportJobStatus::Cancelled;
                job.updated_at = self.clock.now_ms();
                let _ = import::write_job(&self.data_dir, &job);
                drop(job);
                // 审计由异步任务的 `finish_cancelled` 统一写入（避免双写）。
                let _ = import::cleanup_staging(&self.data_dir, &request.job_id);
                self.finish_active(&request.job_id);
                return Ok(json!({"status": "cancelled"}));
            }
        }
        let job = import::read_job(&self.data_dir, &request.job_id).map_err(map_failure)?;
        match job {
            Some(job) => Err(map_failure(ImportFailure::job_not_active(
                &request.job_id,
                job.status,
            ))),
            None => Err(map_failure(ImportFailure::job_not_found(&request.job_id))),
        }
    }

    fn maintenance_state(&self) -> Option<String> {
        if import::pending_exists(&self.data_dir) {
            return Some("存在待重启完成的导入作业（awaiting_restart）".to_owned());
        }
        let active = self.lock_active();
        active.as_ref().and_then(|active| {
            active.job.lock().ok().and_then(|job| {
                if job.status.is_active() {
                    Some(format!(
                        "导入作业 {} 正在执行（{}）",
                        job.job_id,
                        job.status.as_str()
                    ))
                } else {
                    None
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ipc::error::IpcErrorCode;

    #[test]
    fn failure_mapping_uses_registered_business_codes() {
        let mapped = map_failure(ImportFailure::new(
            aether_store::import::ImportFailureKind::ChecksumMismatch,
            "checksum",
        ));
        assert_eq!(mapped.code, IpcErrorCode::InvalidValue);
        assert_eq!(
            mapped.business_code(),
            Some(BusinessCode::ImportChecksumMismatch)
        );
        let mapped = map_failure(ImportFailure::too_large("too large"));
        assert_eq!(mapped.code, IpcErrorCode::TooLarge);
        assert_eq!(mapped.business_code(), Some(BusinessCode::ImportTooLarge));
    }
}
