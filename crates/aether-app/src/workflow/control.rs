//! M10-01/ADR-031：编排 IPC 命令面（11 条）实现。
//!
//! 装饰器位于会话后端外层：命令严格解析在 `ipc::handlers` 完成；本层负责领域校验、
//! 持久化、事件与后台调度。`health` 增量附带 `workflows` 队列/健康指标。

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_control::Clock;
use aether_core::EventType;
use aether_store::{
    ReadPool, StoreCommand, StoreOutcome, WorkflowCommand, WorkflowRecord, WriteQueue,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;

use super::definition::{definition_hash, parse_and_validate, DefinitionError};
use super::engine::{definition_error, CancelOutcome, RetryError, RunRequest, WorkflowEngine};
use crate::ipc::dto::*;
use crate::ipc::path::{self, ArtifactPath};
use crate::ipc::{BusinessCode, IpcBackend, IpcError, IpcErrorCode};

/// 单请求参数转发（M9 既有命令；外层装饰器不得改变语义）。
macro_rules! forward_request {
    ($($name:ident : $ty:ty),* $(,)?) => {
        $(
            fn $name(&self, request: &$ty) -> Result<Value, IpcError> {
                self.inner.$name(request)
            }
        )*
    };
}

/// 无参数/无参数可选返回转发。
macro_rules! forward_none {
    ($($name:ident),* $(,)?) => {
        $(
            fn $name(&self) -> Result<Value, IpcError> {
                self.inner.$name()
            }
        )*
    };
}

/// 编排命令超时（ADR-031 D-031-5：创建/更新/运行/重试 30s，其余 ≤15s）。
const WORKFLOW_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 编排命令后端（装饰器）。
pub struct WorkflowControlBackend {
    inner: Arc<dyn IpcBackend>,
    engine: Arc<WorkflowEngine>,
    reads: ReadPool,
    write: WriteQueue,
    clock: Arc<dyn Clock>,
    handle: Handle,
}

impl WorkflowControlBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        engine: Arc<WorkflowEngine>,
        reads: ReadPool,
        write: WriteQueue,
        clock: Arc<dyn Clock>,
        handle: Handle,
    ) -> Self {
        Self {
            inner,
            engine,
            reads,
            write,
            clock,
            handle,
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
        receiver
            .recv_timeout(WORKFLOW_COMMAND_TIMEOUT)
            .map_err(|_| {
                IpcError::internal(format!(
                    "编排命令超时（>{WORKFLOW_COMMAND_TIMEOUT:?}，核心未在预算内返回）"
                ))
            })?
    }
}

impl IpcBackend for WorkflowControlBackend {
    fn health(&self) -> Result<Value, IpcError> {
        let mut value = self.inner.health()?;
        if let Some(object) = value.as_object_mut() {
            object.insert("workflows".to_owned(), self.engine.metrics());
        }
        Ok(value)
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
        app_restart: AppRestartRequest,
        run_retry: RunRetryRequest,
        runtime_retry: RuntimeRetryRequest,
        runtime_enable: RuntimeEnableRequest,
        runtime_disable: RuntimeDisableRequest,
        workspaces_list: WorkspacesListRequest,
        workspace_update: WorkspaceUpdateRequest,
        workspace_delete: WorkspaceDeleteRequest,
        session_workspace_assign: SessionWorkspaceAssignRequest,
        artifacts_list: ArtifactsListRequest,
        artifact_remove: ArtifactRemoveRequest,
        attachment_list: AttachmentListRequest,
        attachment_remove: AttachmentRemoveRequest,
        providers_list: ProvidersListRequest,
        provider_create: ProviderCreateRequest,
        provider_update: ProviderUpdateRequest,
        provider_delete: ProviderDeleteRequest,
        provider_toggle: ProviderToggleRequest,
        provider_model_add: ProviderModelAddRequest,
        provider_model_toggle: ProviderModelToggleRequest,
        presets_list: PresetsListRequest,
        preset_add: PresetAddRequest,
        preset_update: PresetUpdateRequest,
        preset_delete: PresetDeleteRequest,
        provider_test: ProviderTestRequest,
        provider_models_fetch: ProviderModelsFetchRequest,
        audit_query: AuditQueryRequest,
    );

    forward_none!(runtimes_list, rules_list, backup_list);

    fn attachment_add(
        &self,
        request: &AttachmentAddRequest,
        resolved: &std::path::Path,
    ) -> Result<Value, IpcError> {
        self.inner.attachment_add(request, resolved)
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
        self.inner.session_export(request, canonical_target_dir)
    }

    fn archive_export(
        &self,
        request: &ArchiveExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.archive_export(request, canonical_target_dir)
    }

    fn audit_export(
        &self,
        request: &AuditExportRequest,
        canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.inner.audit_export(request, canonical_target_dir)
    }

    fn workflow_create(&self, request: &WorkflowCreateRequest) -> Result<Value, IpcError> {
        let name = request.name.clone();
        let description = request.description.clone();
        let definition = request.definition.clone();
        let is_template = request.is_template.unwrap_or(false);
        // 借用限制：把依赖克隆进 async 块（句柄均为 Arc/Clone）。
        let reads = self.reads.clone();
        let write = self.write.clone();
        let clock = Arc::clone(&self.clock);
        self.call(async move {
            let helper = Helper {
                reads,
                write,
                clock,
            };
            helper
                .create_workflow(name, description, definition, is_template)
                .await
        })
    }

    fn workflow_list(&self, request: &WorkflowListRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let limit = request.limit.unwrap_or(50);
        let offset = request.offset.unwrap_or(0);
        let include_templates = request.include_templates.unwrap_or(false);
        self.call(async move {
            let items = reads
                .workflows(include_templates, limit, offset)
                .await
                .map_err(|error| IpcError::internal(format!("工作流列表读取失败：{error}")))?;
            let total = reads
                .workflow_count(include_templates)
                .await
                .map_err(|error| IpcError::internal(format!("工作流计数失败：{error}")))?;
            Ok(json!({
                "items": items.iter().map(summary_json).collect::<Vec<_>>(),
                "total": total,
            }))
        })
    }

    fn workflow_get(&self, request: &WorkflowIdRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let workflow_id = request.workflow_id.clone();
        self.call(async move {
            let workflow = reads
                .workflow(&workflow_id, false)
                .await
                .map_err(|error| IpcError::internal(format!("工作流读取失败：{error}")))?
                .ok_or_else(|| workflow_not_found(&workflow_id))?;
            detail_json(&workflow)
        })
    }

    fn workflow_update(&self, request: &WorkflowUpdateRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let clock = Arc::clone(&self.clock);
        let workflow_id = request.workflow_id.clone();
        let expected_version = request.expected_version;
        let name = request.name.clone();
        let description = request.description.clone();
        let definition = request.definition.clone();
        let is_template = request.is_template;
        self.call(async move {
            let existing = reads
                .workflow(&workflow_id, false)
                .await
                .map_err(|error| IpcError::internal(format!("工作流读取失败：{error}")))?
                .ok_or_else(|| workflow_not_found(&workflow_id))?;
            if existing.version != expected_version {
                return Err(workflow_version_conflict());
            }
            let next_definition = match definition {
                Some(definition) => {
                    parse_and_validate(&definition).map_err(definition_error)?;
                    let statuses = vec!["queued".to_owned(), "running".to_owned()];
                    let active = reads
                        .workflow_run_count(Some(&workflow_id), None, &statuses)
                        .await
                        .map_err(|error| IpcError::internal(format!("运行计数失败：{error}")))?;
                    if active > 0 {
                        return Err(workflow_run_active(active));
                    }
                    serde_json::to_string(&definition)
                        .map_err(|error| IpcError::internal(format!("定义序列化失败：{error}")))?
                }
                None => existing.definition.clone(),
            };
            let updated = WorkflowRecord {
                id: existing.id.clone(),
                name: name.unwrap_or(existing.name),
                description: description.or(existing.description),
                version: existing.version.saturating_add(1),
                definition: next_definition,
                is_template: is_template.unwrap_or(existing.is_template),
                created_at: existing.created_at,
                updated_at: clock.now_ms(),
                deleted_at: existing.deleted_at,
            };
            match write
                .execute(StoreCommand::Workflow {
                    command: WorkflowCommand::UpdateWorkflow {
                        workflow: updated.clone(),
                        expected_version,
                    },
                })
                .await
                .map_err(|error| IpcError::internal(format!("工作流更新失败：{error}")))?
            {
                StoreOutcome::WorkflowVersionConflict => Err(workflow_version_conflict()),
                _ => detail_json(&updated),
            }
        })
    }

    fn workflow_delete(&self, request: &WorkflowDeleteRequest) -> Result<Value, IpcError> {
        let write = self.write.clone();
        let reads = self.reads.clone();
        let engine = Arc::clone(&self.engine);
        let clock = Arc::clone(&self.clock);
        let workflow_id = request.workflow_id.clone();
        let force = request.force.unwrap_or(false);
        self.call(async move {
            if force {
                // 先取消本进程内活动运行，避免软删除后后台任务继续改写状态。
                let statuses = vec!["queued".to_owned(), "running".to_owned()];
                let active = reads
                    .workflow_runs(Some(&workflow_id), None, &statuses, 100, 0)
                    .await
                    .map_err(|error| IpcError::internal(format!("运行读取失败：{error}")))?;
                for run in active {
                    let _ = engine.cancel(&run.id, "workflow_deleted").await;
                }
            }
            match write
                .execute(StoreCommand::Workflow {
                    command: WorkflowCommand::DeleteWorkflow {
                        id: workflow_id.clone(),
                        deleted_at: clock.now_ms(),
                        force,
                    },
                })
                .await
                .map_err(|error| IpcError::internal(format!("工作流删除失败：{error}")))?
            {
                StoreOutcome::WorkflowRunActive { runs } => Err(workflow_run_active(runs)),
                StoreOutcome::Applied { affected: 0 } => Err(workflow_not_found(&workflow_id)),
                _ => Ok(json!({"deleted": true})),
            }
        })
    }

    fn workflow_import(&self, request: &WorkflowImportRequest) -> Result<Value, IpcError> {
        let source = path::validate_external_file(&request.source_path, "json")?;
        let metadata = std::fs::metadata(&source)
            .map_err(|error| IpcError::path_rejected(format!("导入文件不可读：{error}")))?;
        if metadata.len() > super::definition::MAX_DEFINITION_BYTES as u64 {
            return Err(IpcError::too_large(
                "source_path",
                format!(
                    "导入文件 {} 字节超过定义上限 {}",
                    metadata.len(),
                    super::definition::MAX_DEFINITION_BYTES
                ),
            ));
        }
        let bytes = std::fs::read(&source)
            .map_err(|error| IpcError::path_rejected(format!("导入文件读取失败：{error}")))?;
        let definition: Value = serde_json::from_slice(&bytes)
            .map_err(|error| IpcError::invalid_value(format!("导入文件不是合法 JSON：{error}")))?;
        let parsed = parse_and_validate(&definition).map_err(definition_error)?;
        let name = request
            .name_override
            .clone()
            .unwrap_or_else(|| parsed.name.clone());
        self.workflow_create(&WorkflowCreateRequest {
            name,
            description: parsed.description.clone(),
            definition,
            is_template: Some(false),
        })
    }

    fn workflow_run(&self, request: &WorkflowRunRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let engine = Arc::clone(&self.engine);
        let clock = Arc::clone(&self.clock);
        let workflow_id = request.workflow_id.clone();
        let session_id = request.session_id.clone();
        let input = request.input.clone().unwrap_or(Value::Null);
        let priority = request.priority.unwrap_or(0);
        self.call(async move {
            let workflow = reads
                .workflow(&workflow_id, false)
                .await
                .map_err(|error| IpcError::internal(format!("工作流读取失败：{error}")))?
                .ok_or_else(|| workflow_not_found(&workflow_id))?;
            let definition_value: Value = serde_json::from_str(&workflow.definition)
                .map_err(|error| IpcError::internal(format!("定义快照损坏：{error}")))?;
            let definition = parse_and_validate(&definition_value).map_err(definition_error)?;
            let session = reads
                .session(
                    &aether_core::SessionId::new(session_id.clone())
                        .map_err(|_| IpcError::invalid_format("session_id", "必须是 26 位 ULID"))?,
                )
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .ok_or_else(|| {
                    IpcError::new(
                        IpcErrorCode::SessionNotFound,
                        format!("会话不存在（{session_id}）"),
                    )
                })?;
            let run_id = ulid::Ulid::new().to_string();
            let request = RunRequest {
                run_id: run_id.clone(),
                workflow_id: workflow.id.clone(),
                session_id: session.id.as_str().to_owned(),
                runtime_id: session.runtime_id.as_str().to_owned(),
                priority,
                input,
                definition,
                definition_value,
            };
            engine
                .start(request)
                .await
                .map_err(|error| IpcError::internal(format!("工作流启动失败：{error}")))?;
            let _ = clock.now_ms();
            Ok(json!({"workflow_run_id": run_id, "status": "queued"}))
        })
    }

    fn workflow_run_get(&self, request: &WorkflowRunGetRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let run_id = request.workflow_run_id.clone();
        let include_nodes = request.include_nodes.unwrap_or(false);
        self.call(async move {
            let run = reads
                .workflow_run(&run_id)
                .await
                .map_err(|error| IpcError::internal(format!("运行读取失败：{error}")))?
                .ok_or_else(|| workflow_run_not_found(&run_id))?;
            let mut value = run_summary_json(&run);
            if include_nodes {
                let nodes = reads
                    .node_runs(&run_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("节点读取失败：{error}")))?;
                let workflow = reads.workflow(&run.workflow_id, true).await.ok().flatten();
                let definition = workflow
                    .as_ref()
                    .and_then(|workflow| serde_json::from_str::<Value>(&workflow.definition).ok())
                    .and_then(|value| parse_and_validate(&value).ok());
                let items = nodes
                    .into_iter()
                    .map(|node| {
                        let node_type = definition
                            .as_ref()
                            .and_then(|definition| definition.node_type(&node.node_id))
                            .map(|node_type| node_type.as_str().to_owned())
                            .unwrap_or_else(|| "unknown".to_owned());
                        json!({
                            "node_run_id": node.id,
                            "node_id": node.node_id,
                            "type": node_type,
                            "status": node.status,
                            "attempt": node.attempt,
                            "started_at": node.started_at,
                            "finished_at": node.finished_at,
                            "error": node.error,
                            "result_summary": node.output.as_deref().map(summarize_result),
                        })
                    })
                    .collect::<Vec<_>>();
                value["nodes"] = Value::Array(items);
            }
            Ok(value)
        })
    }

    fn workflow_run_list(&self, request: &WorkflowRunListRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let workflow_id = request.workflow_id.clone();
        let session_id = request.session_id.clone();
        let status = request.status.clone();
        let limit = request.limit.unwrap_or(50);
        let offset = request.offset.unwrap_or(0);
        self.call(async move {
            let statuses = status.into_iter().collect::<Vec<_>>();
            let runs = reads
                .workflow_runs(
                    workflow_id.as_deref(),
                    session_id.as_deref(),
                    &statuses,
                    limit,
                    offset,
                )
                .await
                .map_err(|error| IpcError::internal(format!("运行列表读取失败：{error}")))?;
            let total = reads
                .workflow_run_count(workflow_id.as_deref(), session_id.as_deref(), &statuses)
                .await
                .map_err(|error| IpcError::internal(format!("运行计数失败：{error}")))?;
            Ok(json!({
                "items": runs.iter().map(run_summary_json).collect::<Vec<_>>(),
                "total": total,
            }))
        })
    }

    fn workflow_run_cancel(&self, request: &WorkflowRunCancelRequest) -> Result<Value, IpcError> {
        let engine = Arc::clone(&self.engine);
        let run_id = request.workflow_run_id.clone();
        let reason = request.reason.clone().unwrap_or_else(|| "user".to_owned());
        self.call(async move {
            match engine.cancel(&run_id, &reason).await {
                Ok(CancelOutcome::Cancelled) => Ok(json!({"status": "cancelled"})),
                Ok(CancelOutcome::Timeout) => Err(IpcError::new(
                    IpcErrorCode::Internal,
                    "工作流取消 5s 未收口；已强制标记 cancelled（cleanup_pending=true）",
                )
                .with_business_code(BusinessCode::WorkflowCancelTimeout)),
                Ok(CancelOutcome::NotActive) => Err(workflow_run_not_active(&run_id)),
                Ok(CancelOutcome::NotFound) => Err(workflow_run_not_found(&run_id)),
                Err(error) => Err(IpcError::internal(format!("取消失败：{error}"))),
            }
        })
    }

    fn workflow_node_retry(&self, request: &WorkflowNodeRetryRequest) -> Result<Value, IpcError> {
        let engine = Arc::clone(&self.engine);
        let run_id = request.workflow_run_id.clone();
        let node_id = request.node_id.clone();
        let reset = request.reset_downstream.unwrap_or(false);
        self.call(async move {
            match engine.retry_node(&run_id, &node_id, reset).await {
                Ok((node_run_id, attempt)) => {
                    Ok(json!({"node_run_id": node_run_id, "attempt": attempt}))
                }
                Err(RetryError::RunNotFound) => Err(workflow_run_not_found(&run_id)),
                Err(RetryError::NodeNotFound) => Err(IpcError::invalid_value(format!(
                    "工作流节点不存在（{node_id}）"
                ))
                .with_business_code(BusinessCode::WorkflowNodeNotFound)),
                Err(RetryError::RunActive) => Err(workflow_run_active(1)),
                Err(RetryError::NodeNotRetryable) => Err(IpcError::invalid_value(format!(
                    "工作流节点不可重试（{node_id}；仅 failed/timeout/cancelled 可重试）"
                ))
                .with_business_code(BusinessCode::WorkflowNodeNotRetryable)),
                Err(RetryError::Store(error)) => {
                    Err(IpcError::internal(format!("节点重试失败：{error}")))
                }
            }
        })
    }
}

/// 供 async 块复用的最小依赖集合（避免捕获 `&self`）。
struct Helper {
    reads: ReadPool,
    write: WriteQueue,
    clock: Arc<dyn Clock>,
}

impl Helper {
    async fn create_workflow(
        &self,
        name: String,
        description: Option<String>,
        definition: Value,
        is_template: bool,
    ) -> Result<Value, IpcError> {
        let parsed = parse_and_validate(&definition).map_err(definition_error)?;
        if !is_template {
            let existing = self
                .reads
                .workflows(true, 500, 0)
                .await
                .map_err(|error| IpcError::internal(format!("工作流列表读取失败：{error}")))?;
            if existing.iter().any(|workflow| workflow.name == name) {
                return Err(IpcError::invalid_value(format!(
                    "工作流名称已存在：{name}；请使用 workflow_update 或改名"
                )));
            }
        }
        let now = self.clock.now_ms();
        let id = ulid::Ulid::new().to_string();
        let record = WorkflowRecord {
            id,
            name,
            description,
            version: 1,
            definition: serde_json::to_string(&definition)
                .map_err(|error| IpcError::internal(format!("定义序列化失败：{error}")))?,
            is_template,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::InsertWorkflow {
                    workflow: record.clone(),
                },
            })
            .await
            .map_err(|error| IpcError::internal(format!("工作流落库失败：{error}")))?;
        let _ = parsed;
        Ok(summary_json(&record))
    }
}

fn summarize_result(output: &str) -> String {
    if output.len() <= 512 {
        return output.to_owned();
    }
    let mut end = 512;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &output[..end])
}

fn summary_json(workflow: &WorkflowRecord) -> Value {
    json!({
        "workflow_id": workflow.id,
        "name": workflow.name,
        "description": workflow.description,
        "version": workflow.version,
        "is_template": workflow.is_template,
        "created_at": workflow.created_at,
        "updated_at": workflow.updated_at,
    })
}

fn detail_json(workflow: &WorkflowRecord) -> Result<Value, IpcError> {
    let definition: Value = serde_json::from_str(&workflow.definition)
        .map_err(|error| IpcError::internal(format!("定义快照损坏：{error}")))?;
    let mut value = summary_json(workflow);
    value["definition"] = definition;
    value["definition_hash"] = Value::String(definition_hash(&value["definition"]));
    Ok(value)
}

fn run_summary_json(run: &aether_store::WorkflowRunRecord) -> Value {
    let context: Value = serde_json::from_str(&run.context).unwrap_or_else(|_| json!({}));
    json!({
        "workflow_run_id": run.id,
        "workflow_id": run.workflow_id,
        "session_id": run.session_id,
        "status": run.status,
        "priority": context.get("priority").and_then(Value::as_i64).unwrap_or(0),
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "error": run.error,
        "definition_version": run.definition_version,
        "definition_hash": run.definition_hash,
    })
}

fn workflow_not_found(workflow_id: &str) -> IpcError {
    IpcError::invalid_value(format!("工作流不存在（{workflow_id}）；已刷新工作流列表"))
        .with_business_code(BusinessCode::WorkflowNotFound)
}

fn workflow_version_conflict() -> IpcError {
    IpcError::invalid_value("工作流定义版本冲突（expected_version 不匹配）；请刷新后重试")
        .with_business_code(BusinessCode::WorkflowVersionConflict)
}

fn workflow_run_active(runs: u64) -> IpcError {
    IpcError::invalid_value(format!("工作流仍有 {runs} 个活动运行；请先取消或等待完成"))
        .with_business_code(BusinessCode::WorkflowRunActive)
}

fn workflow_run_not_found(run_id: &str) -> IpcError {
    IpcError::invalid_value(format!("工作流运行不存在（{run_id}）"))
        .with_business_code(BusinessCode::WorkflowRunNotFound)
}

fn workflow_run_not_active(run_id: &str) -> IpcError {
    IpcError::invalid_value(format!("工作流运行不在活动状态（{run_id}）"))
        .with_business_code(BusinessCode::WorkflowRunNotActive)
}

// 事件类型引用保持编译期覆盖（workflow.* 精确子类型由引擎发射）。
#[allow(dead_code)]
const _EVENT_TYPES: [EventType; 9] = [
    EventType::WorkflowStarted,
    EventType::WorkflowNodeStarted,
    EventType::WorkflowNodeCompleted,
    EventType::WorkflowNodeFailed,
    EventType::WorkflowNodeCancelled,
    EventType::WorkflowNodeSkipped,
    EventType::WorkflowCompleted,
    EventType::WorkflowFailed,
    EventType::WorkflowCancelled,
];

#[allow(dead_code)]
fn _definition_error_is_used(error: DefinitionError) -> IpcError {
    definition_error(error)
}
