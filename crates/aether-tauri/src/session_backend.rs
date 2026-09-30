//! 会话命令与消息分页的真实后端（M3-02；M3-03 增加权限中心命令面）。
//!
//! 覆盖 D7 命令面中的会话族与消息分页：
//! - `session_list` / `session_create` / `session_send` / `session_interrupt` /
//!   `session_dispose`：薄适配 [`aether_control::SessionManager`]（状态机/run 串行/
//!   幂等/ack 快路径语义全部在控制层，本层不复刻）；`session_create` 额外拒绝
//!   `disabled` 运行时（D5：`disabled` 不可用于新会话，M3-03 DoD3）；
//! - `messages_page`：双层契约（详见 [`MessagesPageResponse`]）——按 `last_seq` 读
//!   **events 表**（D4 补读语义；`messages.seq` 与 `events.seq` 是两条独立序列，
//!   事件流连续性只能由 events 表承载）；`last_seq` 缺省返回**尾部**一页事件
//!   （升序）并叠加 `messages` 表**尾部**一页消息基线（升序；仅该分支返回）。
//!   缺口 > [`aether_control::READBACK_MAX_GAP`]（10k）→ `readback_gap_too_large`
//!   同码透传（ADR-009 决策 2）；
//! - `runtimes_list`：监督器注册表快照（含 hello 上报的能力清单）；
//! - `permissions_pending` / `permission_resolve`（M3-03）：薄适配
//!   [`aether_control::PermissionService`]（策略/审批队列/300s 超时/审计语义全部在
//!   控制层，本层不复刻）；响应形状与错误映射见本模块文档与 M3-03 证据。
//!
//! 桥接口径与 `runtime_control` 一致：async 服务 spawn 到核心运行时 + `std::sync::mpsc`
//! 同步等待（不阻塞运行时线程），30s 硬超时。

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_adapters::connection::ConnectionState;
use aether_adapters::supervisor::Supervisor;
use aether_control::{
    compose_memory_injection, LifecycleError, MemoryInjection, PermissionError, PermissionService,
    SessionManager, READBACK_MAX_GAP,
};
use aether_core::{
    EventEnvelope, Message, PermissionDecision, PermissionScope, Runtime, RuntimeId, RuntimeStatus,
    SessionId, SessionStatus, Workspace, WorkspaceId, THINKING_DEPTH_CAPABILITY,
    THINKING_DEPTH_DEFAULT,
};
use aether_store::{
    ArtifactRecord, ReadPool, SessionQuery, StoreCommand, StoreOutcome, WriteQueue,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    ArtifactAddRequest, ArtifactRemoveRequest, ArtifactsListRequest, MessagesPageRequest,
    PermissionResolveRequest, PermissionsPendingRequest, ProviderCreateRequest,
    ProviderDeleteRequest, ProviderModelAddRequest, ProviderModelToggleRequest,
    ProviderToggleRequest, ProviderUpdateRequest, ProvidersListRequest, SessionCreateRequest,
    SessionIdRequest, SessionListRequest, SessionSendRequest,
};
use crate::ipc::error::{IpcError, IpcErrorCode};
use crate::ipc::path::ArtifactPath;
use crate::json_payload::JsonPayload;

use crate::adapter_executor::AdapterRunExecutor;
use crate::provider_control::ProviderControl;

/// 会话命令硬超时（`session.create`/`session.send` 含适配器调用预算，D6 方法表 30s）。
pub const SESSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// `messages_page` 响应（ADR-009 决策 1；字段表见下）。
///
/// 双层契约（两条独立 seq 序列，仅 `events` 承载补读水位）：
/// - `events`：D4 补读语义（`last_seq` 断点 / 最近一页）→ `EventBackfillSource`；
///   缺省分支取**尾部** `limit` 条（升序，D4「上限 10k」为缺口上限而非页大小）；
/// - `messages`：会话消息历史（`messages` 表**尾部** `limit` 条，升序）→ 工作台消息
///   基线。**仅最近一页（`last_seq` 缺省）返回**：无消息时为 `Some([])`（字段存在为
///   `[]`）；补读热路径为 `None`（字段省略），不读消息表。
///
/// `complete` 边界：补读分支为「末条 seq == max_seq **或** `last_seq >= max_seq`」
/// （后者视为已到最新，返回空 `events`）；`max_seq` 缺失与最近一页分支恒 `true`。
#[derive(Debug, Clone, Serialize)]
pub struct MessagesPageResponse {
    pub session_id: String,
    /// 请求断点（`None` = 最近一页）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    /// 会话当前最大 seq（无事件为 `null`）。
    pub max_seq: Option<u64>,
    pub events: Vec<EventEnvelope>,
    /// 会话消息历史（升序；仅最近一页返回；无消息为 `Some([])`，补读页为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    /// 本页是否已到最新（`false` = 可能仍有后续缺口，调用方续读）。
    pub complete: bool,
}

/// 非阻断警告条目（ADR-010 决策 4：`warnings[].code` 为独立命名空间，非
/// `IpcErrorCode`；登记位 = ADR-006 附录 B「警告码」子表，首项
/// `thinking_depth_unsupported`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct SessionWarning {
    /// 警告码（稳定契约；当前唯一取值 `thinking_depth_unsupported`）。
    pub code: String,
    /// 关联请求字段。
    pub field: String,
    /// 关联运行时 id。
    pub runtime_id: String,
    /// 展示文案（不参与逻辑判断）。
    pub message: String,
}

/// `session_list` / `session_create` 响应元素定型 DTO（ADR-010 附录 B.4）。
///
/// 字段与核心 `Session` 域实体一一对应；新增可选 `workspace_root`（由
/// `sessions.workspace_id` → `workspaces.root_path` 解析，**未绑定工作区省略**；
/// 只出现在响应、不进事件 payload）与可选 `thinking_depth`（会话级档位，M3-10）。
/// 可选 `warnings` 为 `session_create` 同步能力门判定结果（ADR-010 决策 2：
/// 尽力而为交付；`session_list` 不回填、无警告省略）。
///
/// `config` / `token_usage` 用 [`JsonPayload`] 透传（与 `AetherEvent.payload` 同口径：
/// specta rc.25 对 `serde_json::Value` 的内联递归定义会栈溢出）。
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct SessionSummary {
    pub id: String,
    pub runtime_id: String,
    pub workspace_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub title: String,
    pub status: crate::ipc::dto::SessionStatus,
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_depth: Option<u8>,
    pub system_prompt: Option<String>,
    pub config: JsonPayload,
    pub token_usage: JsonPayload,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<SessionWarning>>,
}

impl SessionSummary {
    /// 由核心会话实体构造；`workspace_root` 为调用方解析的工作区根（无绑定/无行 = None）。
    pub fn from_session(
        session: &aether_core::Session,
        workspace_root: Option<String>,
    ) -> Result<Self, IpcError> {
        let token_usage = serde_json::to_value(session.token_usage)
            .map_err(|error| IpcError::internal(format!("token_usage 序列化失败：{error}")))?;
        Ok(Self {
            id: session.id.as_str().to_owned(),
            runtime_id: session.runtime_id.as_str().to_owned(),
            workspace_id: session
                .workspace_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            parent_session_id: session
                .parent_session_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            title: session.title.clone(),
            status: session_status_to_dto(session.status),
            model: session.model.clone(),
            thinking_depth: Some(session.thinking_depth),
            system_prompt: session.system_prompt.clone(),
            config: JsonPayload(session.config.clone()),
            token_usage: JsonPayload(token_usage),
            created_at: session.created_at,
            updated_at: session.updated_at,
            closed_at: session.closed_at,
            workspace_root,
            warnings: None,
        })
    }

    /// 同步能力门警告（`session_create`：runtime ready 且未声明 `thinking_depth`）。
    pub fn with_warnings(mut self, warnings: Option<Vec<SessionWarning>>) -> Self {
        self.warnings = warnings;
        self
    }
}

/// 能力门同步判定（ADR-010 决策 2）：runtime ready 且未声明 `thinking_depth` 能力时
/// 返回 `warnings[0].code="thinking_depth_unsupported"`（非阻断；延迟判定路径——
/// runtime 未 ready——不产生响应警告，以 `SessionSummary.thinking_depth` 回显为准）。
pub fn thinking_depth_warning(
    runtime_ready: bool,
    capabilities: &[String],
    runtime_id: &str,
) -> Option<Vec<SessionWarning>> {
    if runtime_ready && !capabilities.iter().any(|item| item == THINKING_DEPTH_CAPABILITY) {
        return Some(vec![SessionWarning {
            code: "thinking_depth_unsupported".to_owned(),
            field: "thinking_depth".to_owned(),
            runtime_id: runtime_id.to_owned(),
            message: "当前运行时不支持思考深度，已按默认档位运行".to_owned(),
        }]);
    }
    None
}

/// 工作区绑定（M3-08；设计 D14/ADR-004 决策 3）。
///
/// 语义：`root_path` 为 canonical 路径；`workspace_set` 后**新会话**按该工作区注入
/// 记忆文件并把权限基准目录换绑到同一根（`PermissionService` 策略引擎重建）；
/// 已有会话的 `sessions.workspace_id` 不迁移（P0 口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub id: WorkspaceId,
    pub name: String,
    pub root_path: std::path::PathBuf,
}

impl WorkspaceBinding {
    fn from_row(workspace: &Workspace) -> Self {
        Self {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
            root_path: std::path::PathBuf::from(&workspace.root_path),
        }
    }

    /// 响应形状（`workspace_set` 回执；字段与 `workspaces` 表一致）。
    fn to_json(&self) -> Value {
        json!({
            "workspace_id": self.id.as_str(),
            "name": self.name,
            "root_path": self.root_path.to_string_lossy(),
        })
    }
}

/// 会话命令后端（装饰器：其余命令透传内层）。
pub struct SessionBackend {
    inner: Arc<dyn IpcBackend>,
    manager: Option<SessionManager>,
    executor: Option<Arc<AdapterRunExecutor>>,
    reads: Option<ReadPool>,
    supervisor: Option<Arc<Supervisor>>,
    /// 权限服务（M3-03；`None` = 未接线 → 权限命令回 `core_not_ready`）。
    permissions: Option<PermissionService>,
    /// 工作区写路径（M3-08 `workspace_set` 的 `workspaces` 落库；`None` = 未接线）。
    write: Option<WriteQueue>,
    /// 供应商命令执行体（M3-11；`None` = 未接线 → 供应商命令回 `core_not_ready`）。
    providers: Option<ProviderControl>,
    /// 当前工作区绑定（M3-08；`None` = 未绑定）。
    workspace: Arc<Mutex<Option<WorkspaceBinding>>>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    /// 自动补读缺口上限（默认 [`READBACK_MAX_GAP`]；常量级调参，测试注入用）。
    gap_limit: u64,
}

impl SessionBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        manager: Option<SessionManager>,
        executor: Option<Arc<AdapterRunExecutor>>,
        reads: Option<ReadPool>,
        supervisor: Option<Arc<Supervisor>>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner,
            manager,
            executor,
            reads,
            supervisor,
            permissions: None,
            write: None,
            providers: None,
            workspace: Arc::new(Mutex::new(None)),
            handle,
            timeout: SESSION_COMMAND_TIMEOUT,
            gap_limit: READBACK_MAX_GAP,
        }
    }

    /// 注入权限服务（M3-03；`permissions_pending` / `permission_resolve` 的真实数据源）。
    #[must_use]
    pub fn with_permissions(mut self, permissions: PermissionService) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// 注入存储写路径（M3-08 `workspace_set` 的 `workspaces` 落库与换绑；
    /// M3-09 `artifact_add`/`artifact_remove` 的 `artifacts` 落库复用同一队列）。
    #[must_use]
    pub fn with_workspace_store(mut self, write: WriteQueue) -> Self {
        self.write = Some(write);
        self
    }

    /// 注入供应商命令执行体（M3-11：`providers_list` / `provider_*` 七命令）。
    #[must_use]
    pub fn with_providers(mut self, providers: ProviderControl) -> Self {
        self.providers = Some(providers);
        self
    }

    /// 供应商命令执行体（未接线 → 供应商命令回 `core_not_ready`）。
    fn providers_required(&self) -> Result<&ProviderControl, IpcError> {
        self.providers.as_ref().ok_or_else(|| {
            IpcError::core_not_ready(
                "供应商后端未接线：providers_list / provider_* 不可用（启动序列未完成）",
            )
        })
    }

    /// 存储写路径（未接线 → 引用写命令回 `core_not_ready`）。
    fn write_required(&self) -> Result<&WriteQueue, IpcError> {
        self.write.as_ref().ok_or_else(|| {
            IpcError::core_not_ready(
                "存储写路径未接线：artifact_add / artifact_remove 不可用（启动序列未完成）",
            )
        })
    }

    /// 当前工作区绑定（诊断/测试；`None` = 未绑定）。
    pub fn workspace_binding(&self) -> Option<WorkspaceBinding> {
        lock_workspace(&self.workspace).clone()
    }

    /// 启动恢复：最近绑定的工作区（M3-08；无绑定/根不存在 → 不绑定，权限基准保持
    /// 启动默认）。在核心启动序列内调用一次（存储/权限服务就绪后）。
    pub async fn restore_workspace_binding(&self) -> Result<Option<WorkspaceBinding>, IpcError> {
        let Some(reads) = &self.reads else {
            return Ok(None);
        };
        let Some(workspace) = reads
            .workspaces_latest()
            .await
            .map_err(|error| IpcError::internal(format!("工作区恢复读取失败：{error}")))?
        else {
            return Ok(None);
        };
        let binding = WorkspaceBinding::from_row(&workspace);
        if !binding.root_path.is_dir() {
            tracing::warn!(
                workspace_id = %binding.id,
                root_path = %binding.root_path.display(),
                "工作区根不存在：跳过启动绑定（权限基准保持启动默认）"
            );
            return Ok(None);
        }
        if let Some(service) = &self.permissions {
            service
                .set_workspace_root(&binding.root_path)
                .map_err(|error| {
                    IpcError::invalid_value(format!("工作区根不可用（策略引擎换根失败）：{error}"))
                })?;
        }
        *lock_workspace(&self.workspace) = Some(binding.clone());
        Ok(Some(binding))
    }

    /// 覆盖自动补读缺口上限（测试/故障注入；默认 `READBACK_MAX_GAP`）。
    #[must_use]
    pub fn with_gap_limit(mut self, gap_limit: u64) -> Self {
        self.gap_limit = gap_limit;
        self
    }

    /// 当前缺口上限（诊断）。
    pub fn gap_limit(&self) -> u64 {
        self.gap_limit
    }

    fn manager_required(&self) -> Result<&SessionManager, IpcError> {
        self.manager.as_ref().ok_or_else(|| {
            IpcError::core_not_ready("会话后端未接线：启动序列尚未完成存储/管线注入")
        })
    }

    fn reads_required(&self) -> Result<&ReadPool, IpcError> {
        self.reads.as_ref().ok_or_else(|| {
            IpcError::core_not_ready("会话后端未接线：read 连接池不可用（启动失败或未完成）")
        })
    }

    fn permissions_required(&self) -> Result<&PermissionService, IpcError> {
        self.permissions.as_ref().ok_or_else(|| {
            IpcError::core_not_ready(
                "权限服务未接线：permissions_pending / permission_resolve 不可用",
            )
        })
    }

    /// 同步桥接：spawn 到核心运行时并等待（与 `SupervisorControl::call` 同口径）。
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
                "会话命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }
}

impl IpcBackend for SessionBackend {
    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        let supervisor = self.supervisor.clone().ok_or_else(|| {
            IpcError::core_not_ready("监督器未接线：runtimes_list 不可用（启动序列未完成）")
        })?;
        self.call(async move {
            let mut runtimes = Vec::new();
            for id in supervisor.runtime_ids() {
                let Some(runtime) = supervisor.get(&id) else {
                    continue;
                };
                let (status, reason) = runtime.summary_snapshot();
                let manifest = runtime.manifest();
                let capabilities = match runtime.connection().await {
                    Some(connection) => match connection.state() {
                        ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => {
                            hello.runtime.capabilities
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                };
                runtimes.push(json!({
                    "id": manifest.id,
                    "name": manifest.name,
                    "kind": manifest.kind,
                    "version": manifest.version,
                    "protocol": manifest.protocol,
                    "capabilities": capabilities,
                    "enabled": manifest.enabled,
                    "status": status.as_str(),
                    "status_reason": reason.map(|reason| reason.as_str()),
                }));
            }
            runtimes.sort_by(|left, right| {
                left["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(right["id"].as_str().unwrap_or(""))
            });
            Ok(Value::Array(runtimes))
        })
    }

    /// ADR-010 附录 B.4：响应元素定型为 [`SessionSummary`]（新增可选 `workspace_root`，
    /// 由 `sessions.workspace_id` → `workspaces.root_path` 解析；未绑定省略）。
    fn session_list(&self, request: &SessionListRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let query = SessionQuery {
            runtime_id: request.runtime_id.clone(),
            status: request.status.map(map_session_status),
            parent_session_id: None,
            limit: request.limit,
        };
        self.call(async move {
            let sessions = reads
                .sessions(query)
                .await
                .map_err(|error| IpcError::internal(format!("会话列表读取失败：{error}")))?;
            // 工作区根解析：按 workspace_id 去重读取（会话数有界，含工作区行时一次读）。
            let mut roots: std::collections::HashMap<String, Option<String>> =
                std::collections::HashMap::new();
            for session in &sessions {
                let Some(workspace_id) = &session.workspace_id else {
                    continue;
                };
                let key = workspace_id.as_str().to_owned();
                if roots.contains_key(&key) {
                    continue;
                }
                let root = reads
                    .workspace(workspace_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                    .map(|workspace| workspace.root_path);
                roots.insert(key, root);
            }
            let summaries = sessions
                .iter()
                .map(|session| {
                    let root = session
                        .workspace_id
                        .as_ref()
                        .and_then(|id| roots.get(id.as_str()).cloned().flatten());
                    SessionSummary::from_session(session, root)
                })
                .collect::<Result<Vec<_>, _>>()?;
            serde_json::to_value(summaries)
                .map_err(|error| IpcError::internal(format!("会话列表序列化失败：{error}")))
        })
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let supervisor = self.supervisor.clone();
        let reads = self.reads.clone();
        let binding = lock_workspace(&self.workspace).clone();
        let runtime_id = request.runtime_id.clone();
        let title = request.title.clone();
        let workspace_id = request.workspace_id.clone();
        let model = request.model.clone();
        let thinking_depth_request = request.thinking_depth_value()?;
        self.call(async move {
            let runtime = {
                let supervisor = supervisor.ok_or_else(|| {
                    IpcError::core_not_ready("监督器未接线：无法解析 runtime_id（启动序列未完成）")
                })?;
                let runtime = supervisor.get(&runtime_id).ok_or_else(|| {
                    IpcError::invalid_enum(
                        "runtime_id",
                        format!("runtime_id {runtime_id:?} 不在注册表（监督器白名单）"),
                    )
                })?;
                let (status, reason) = runtime.summary_snapshot();
                // M3-03 DoD3（D5）：`disabled` 运行时不可创建会话；修复后经
                // `runtime_enable` / `runtime_retry` 恢复。校验在命令层委托的此处，
                // 防止 UI 之外的调用路径绕过。
                if status == RuntimeStatus::Disabled {
                    let detail = reason.map(|reason| reason.as_str()).unwrap_or("disabled");
                    return Err(IpcError::invalid_value(format!(
                        "runtime {runtime_id:?} 已禁用（{detail}），不可创建会话；请先修复并重新启用"
                    )));
                }
                let manifest = runtime.manifest();
                // `connection_ready`：ADR-010 能力门判定时机——runtime ready（连接
                // Ready/Degraded）时同步判定；cold/starting（Connecting/无连接）接受
                // 请求并按延迟判定处理（会话照常创建，不做响应警告）。
                let (version, capabilities, connection_ready) = match runtime.connection().await {
                    Some(connection) => match connection.state() {
                        ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => {
                            (hello.runtime.version, hello.runtime.capabilities, true)
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            (manifest.version.clone(), Vec::new(), false)
                        }
                    },
                    None => (manifest.version.clone(), Vec::new(), false),
                };
                let now = now_ms();
                let runtime_row = Runtime {
                    id: RuntimeId::new(manifest.id.clone()).map_err(|error| {
                        IpcError::internal(format!("runtime_id 非法（监督器 manifest）：{error}"))
                    })?,
                    name: manifest.name.clone(),
                    kind: manifest.kind.clone(),
                    version,
                    protocol: manifest.protocol.clone(),
                    capabilities,
                    endpoint: Some(manifest.program.display().to_string()),
                    config: json!({}),
                    status,
                    status_reason: reason.map(|reason| reason.as_str().to_owned()),
                    last_seen_at: None,
                    created_at: now,
                    updated_at: now,
                };
                (runtime_row, connection_ready)
            };
            let (runtime, connection_ready) = runtime;
            // M3-08/D14：解析工作区（显式 `workspace_id` 白名单校验 → 行存在；缺省 =
            // 当前绑定），按工作区根组合记忆注入（优先级 + 32KB 截断 + 显式标记）。
            let resolved_workspace = match workspace_id {
                Some(value) => {
                    let id = WorkspaceId::new(value).map_err(|error| {
                        IpcError::internal(format!("workspace_id 非法：{error}"))
                    })?;
                    let reads = reads.clone().ok_or_else(|| {
                        IpcError::core_not_ready(
                            "会话后端未接线：workspace_id 存在性校验需要读连接池",
                        )
                    })?;
                    let workspace = reads
                        .workspace(&id)
                        .await
                        .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                        .ok_or_else(|| {
                            IpcError::invalid_value(format!(
                                "workspace_id 不存在（{}）；请先经 workspace_set 绑定",
                                id.as_str()
                            ))
                        })?;
                    Some(workspace)
                }
                None => binding.as_ref().map(|binding| Workspace {
                    id: binding.id.clone(),
                    name: binding.name.clone(),
                    root_path: binding.root_path.to_string_lossy().to_string(),
                    memory_files: Vec::new(),
                    created_at: 0,
                    updated_at: 0,
                }),
            };
            let (workspace_id, system_prompt) = match &resolved_workspace {
                Some(workspace) => {
                    let injection: MemoryInjection =
                        compose_memory_injection(Path::new(&workspace.root_path));
                    if let Some(error) = &injection.read_error {
                        tracing::warn!(
                            workspace_id = %workspace.id,
                            error = %error,
                            "记忆文件候选读取诊断（继续会话创建）"
                        );
                    }
                    (Some(workspace.id.clone()), injection.render())
                }
                None => (None, None),
            };
            // M3-10/ADR-010 能力门（同步判定路径）：runtime ready 且未声明
            // `thinking_depth` → 会话级落缺省 2 + 响应携带非阻断警告（尽力而为）；
            // ready 且声明 → 维持请求值；runtime 未 ready → 接受请求并按延迟判定
            // （会话照常创建；无响应警告，以 `SessionSummary.thinking_depth` 回显为准，
            // run 启动时由执行器再判定）。
            let warnings = thinking_depth_warning(
                connection_ready,
                &runtime.capabilities,
                runtime.id.as_str(),
            );
            let effective_depth = if connection_ready && warnings.is_some() {
                THINKING_DEPTH_DEFAULT
            } else {
                thinking_depth_request.unwrap_or(THINKING_DEPTH_DEFAULT)
            };
            let session = manager
                .create_session_with_depth(
                    runtime,
                    &title,
                    workspace_id,
                    model,
                    system_prompt,
                    effective_depth,
                )
                .await
                .map_err(map_lifecycle_error)?;
            // ADR-010 附录 B.4：响应定型为 `SessionSummary`（`workspace_root` 取自
            // 本会话解析出的工作区；未绑定省略；`thinking_depth` 回显生效值）。
            let workspace_root = resolved_workspace
                .as_ref()
                .map(|workspace| workspace.root_path.clone());
            let summary =
                SessionSummary::from_session(&session, workspace_root)?.with_warnings(warnings);
            serde_json::to_value(summary)
                .map_err(|error| IpcError::internal(format!("会话序列化失败：{error}")))
        })
    }

    /// M3-08 `workspace_set`：绑定/切换工作区（ADR-004 决策 3 / D14）。
    ///
    /// - `workspace_id`：ULID 形态已由 DTO 校验；存在性在此复核（不存在 → `invalid_value`）；
    /// - `root_path`：canonical 路径（命令层已做存在目录 + 同步盘拒绝）→ 复用同路径既有行，
    ///   否则新建 `workspaces` 行（经单写队列，AGENTS §2.4）；重复绑定幂等；
    /// - 换绑：更新进程内绑定；`PermissionService` 策略引擎换根到同一 canonical 路径
    ///   （权限基准与工作区同源，M3-08 DoD7）；已有会话不迁移（P0）。
    fn workspace_set(
        &self,
        request: &crate::ipc::dto::WorkspaceSetRequest,
        canonical_root_path: Option<&std::path::Path>,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write.clone().ok_or_else(|| {
            IpcError::core_not_ready("工作区写路径未接线：workspace_set 不可用（启动序列未完成）")
        })?;
        let permissions = self.permissions.clone();
        let binding_slot = Arc::clone(&self.workspace);
        let workspace_id = request.workspace_id.clone();
        let canonical_root = canonical_root_path.map(std::path::Path::to_path_buf);
        self.call(async move {
            let workspace = match workspace_id {
                Some(value) => {
                    let id = WorkspaceId::new(value).map_err(|error| {
                        IpcError::internal(format!("workspace_id 非法：{error}"))
                    })?;
                    reads
                        .workspace(&id)
                        .await
                        .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                        .ok_or_else(|| {
                            IpcError::invalid_value(format!(
                                "workspace_id 不存在（{}）；无法绑定未知工作区",
                                id.as_str()
                            ))
                        })?
                }
                None => {
                    let root = canonical_root.ok_or_else(|| {
                        IpcError::invalid_value("必须且只能提供 workspace_id 或 root_path 之一")
                    })?;
                    let root_path = root.to_string_lossy().to_string();
                    match reads.workspace_by_root(&root_path).await.map_err(|error| {
                        IpcError::internal(format!("工作区按路径读取失败：{error}"))
                    })? {
                        Some(existing) => existing,
                        None => {
                            let now = now_ms();
                            let name = root
                                .file_name()
                                .and_then(|value| value.to_str())
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned)
                                .unwrap_or_else(|| root_path.clone());
                            let workspace = Workspace {
                                id: WorkspaceId::new(ulid::Ulid::new().to_string()).map_err(
                                    |error| {
                                        IpcError::internal(format!(
                                            "workspace_id 生成失败：{error}"
                                        ))
                                    },
                                )?,
                                name,
                                root_path,
                                // P0 记忆文件清单固定为 D14 三文件名（内置优先级），
                                // 本列保留用于未来自定义白名单（P1 设置项）。
                                memory_files: Vec::new(),
                                created_at: now,
                                updated_at: now,
                            };
                            let outcome = write
                                .execute(StoreCommand::UpsertWorkspace {
                                    workspace: workspace.clone(),
                                })
                                .await
                                .map_err(|error| {
                                    IpcError::internal(format!("工作区落库失败：{error}"))
                                })?;
                            if let StoreOutcome::Applied { affected } = outcome {
                                if affected == 0 {
                                    return Err(IpcError::internal(
                                        "工作区落库未影响任何行（不变量破坏）",
                                    ));
                                }
                            }
                            workspace
                        }
                    }
                }
            };
            let binding = WorkspaceBinding::from_row(&workspace);
            // 权限基准目录与工作区同源（策略引擎换根；换绑失败 → 不更新绑定，
            // 避免注入基准与权限基准不一致）。
            if let Some(service) = &permissions {
                service
                    .set_workspace_root(&binding.root_path)
                    .map_err(|error| {
                        IpcError::invalid_value(format!(
                            "工作区根不可用（策略引擎换根失败）：{error}"
                        ))
                    })?;
            }
            *lock_workspace(&binding_slot) = Some(binding.clone());
            Ok(binding.to_json())
        })
    }

    /// M3-09 `artifacts_list`（ADR-010 决策 1）：会话引用清单（按 `created_at` 升序）。
    ///
    /// 只读：不触发 `workspace_set`、不产生事件；不存在会话 → `invalid_value`。
    fn artifacts_list(&self, request: &ArtifactsListRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let session_id = parse_session_id(&request.session_id)?;
        self.call(async move {
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?;
            if session.is_none() {
                return Err(session_not_found(&session_id));
            }
            let artifacts = reads
                .artifacts(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("引用清单读取失败：{error}")))?;
            Ok(json!({
                "artifacts": artifacts.iter().map(artifact_json).collect::<Vec<_>>(),
            }))
        })
    }

    /// M3-09 `artifact_add`（ADR-010 决策 1）：登记会话引用。
    ///
    /// `resolved` 为命令层 canonicalize + 可访问性检查结果（失败已在命令层以
    /// `artifact_path_rejected` 拒绝）；写路径经单写队列（D3）；
    /// 同会话同路径幂等（`UNIQUE(session_id, path)` 兜底）。
    fn artifact_add(
        &self,
        request: &ArtifactAddRequest,
        resolved: &ArtifactPath,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let session_id = parse_session_id(&request.session_id)?;
        let path = resolved.canonical.to_string_lossy().to_string();
        let kind = resolved.kind.to_owned();
        let size_bytes = resolved.size_bytes;
        self.call(async move {
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?;
            if session.is_none() {
                return Err(session_not_found(&session_id));
            }
            let artifact = ArtifactRecord {
                id: ulid::Ulid::new().to_string(),
                session_id: session_id.as_str().to_owned(),
                path,
                kind,
                size_bytes,
                created_at: now_ms(),
            };
            let outcome = write
                .execute(StoreCommand::InsertArtifact { artifact })
                .await
                .map_err(|error| IpcError::internal(format!("引用落库失败：{error}")))?;
            match outcome {
                StoreOutcome::ArtifactRecorded { artifact, .. } => Ok(artifact_json(&artifact)),
                other => Err(IpcError::internal(format!(
                    "引用落库返回了非预期结果：{other:?}"
                ))),
            }
        })
    }

    /// M3-09 `artifact_remove`（ADR-010 决策 1）：删除会话引用；不存在 →
    /// 幂等 `{ removed: false }`（不新增错误码）。
    fn artifact_remove(&self, request: &ArtifactRemoveRequest) -> Result<Value, IpcError> {
        let write = self.write_required()?.clone();
        let session_id = parse_session_id(&request.session_id)?;
        let artifact_id = request.artifact_id.clone();
        self.call(async move {
            let outcome = write
                .execute(StoreCommand::RemoveArtifact {
                    session_id,
                    artifact_id,
                })
                .await
                .map_err(|error| IpcError::internal(format!("引用删除失败：{error}")))?;
            match outcome {
                StoreOutcome::Applied { affected } => Ok(json!({ "removed": affected > 0 })),
                other => Err(IpcError::internal(format!(
                    "引用删除返回了非预期结果：{other:?}"
                ))),
            }
        })
    }

    // ===== M3-11（ADR-010 决策 3）：模型与供应商配置七命令（薄转发 [`ProviderControl`]；
    // 校验/密钥写入/落库语义全部在 provider_control，本层不复刻）。=====

    fn providers_list(&self, request: &ProvidersListRequest) -> Result<Value, IpcError> {
        self.providers_required()?.providers_list(request)
    }

    fn provider_create(&self, request: &ProviderCreateRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_create(request)
    }

    fn provider_update(&self, request: &ProviderUpdateRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_update(request)
    }

    fn provider_delete(&self, request: &ProviderDeleteRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_delete(request)
    }

    fn provider_toggle(&self, request: &ProviderToggleRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_toggle(request)
    }

    fn provider_model_add(&self, request: &ProviderModelAddRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_model_add(request)
    }

    fn provider_model_toggle(
        &self,
        request: &ProviderModelToggleRequest,
    ) -> Result<Value, IpcError> {
        self.providers_required()?.provider_model_toggle(request)
    }

    fn session_send(&self, request: &SessionSendRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let reads = self.reads.clone();
        let supervisor = self.supervisor.clone();
        let session_id = request.session_id.clone();
        let text = request.text.clone();
        let client_msg_id = request.client_msg_id.clone();
        let thinking_override = request.thinking_depth_value()?;
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            // M3-10/ADR-010 能力门（同步判定路径，尽力而为）：run 执行前的 runtime
            // ready 快照；未 ready（cold/starting）→ 延迟判定（不产生响应警告，
            // run 启动时由执行器按同一门再判定）。覆盖请求仅影响本次 run。
            let warnings = match (&reads, &supervisor) {
                (Some(reads), Some(supervisor)) => {
                    let mut warnings = None;
                    if let Some(session) = reads
                        .session(&session_id)
                        .await
                        .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                    {
                        if let Some(runtime) = supervisor.get(session.runtime_id.as_str()) {
                            if let Some(connection) = runtime.connection().await {
                                let (capabilities, ready) = match connection.state() {
                                    ConnectionState::Ready(hello)
                                    | ConnectionState::Degraded { hello, .. } => {
                                        (hello.runtime.capabilities, true)
                                    }
                                    ConnectionState::Connecting
                                    | ConnectionState::Disconnected(_) => (Vec::new(), false),
                                };
                                warnings = thinking_depth_warning(
                                    ready,
                                    &capabilities,
                                    session.runtime_id.as_str(),
                                );
                            }
                        }
                    }
                    warnings
                }
                _ => None,
            };
            let ack = manager
                .send_with_thinking_depth(
                    &session_id,
                    &text,
                    &client_msg_id,
                    thinking_override,
                )
                .await
                .map_err(map_lifecycle_error)?;
            let mut response = json!({
                "session_id": ack.session_id.as_str(),
                "message_id": ack.message_id.as_str(),
                "run_id": ack.run_id.as_str(),
                "queued": ack.queued,
                "duplicate": ack.duplicate,
            });
            if let Some(warnings) = warnings {
                response["warnings"] = serde_json::to_value(warnings)
                    .map_err(|error| IpcError::internal(format!("警告序列化失败：{error}")))?;
            }
            Ok(response)
        })
    }

    fn session_interrupt(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let report = manager
                .interrupt(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": report.session_id.as_str(),
                "interrupted_run": report.interrupted_run.as_ref().map(|run| run.as_str()),
                "cancelled_waiting_run": report.cancelled_waiting_run.as_ref().map(|run| run.as_str()),
            }))
        })
    }

    fn session_dispose(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let executor = self.executor.clone();
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let status = manager
                .dispose(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            if let Some(executor) = executor {
                executor.dispose_session(&session_id).await;
            }
            Ok(json!({
                "session_id": session_id.as_str(),
                "status": status.as_str(),
            }))
        })
    }

    /// M3-06 `run_retry`：一键重放（仅终态 run；重放按 ADR-005 Mode R/N，由执行器
    /// 按 `sessions.config.native_id` 与适配器能力决定）。产生新 run 且旧 run 保留审计。
    fn run_retry(&self, request: &crate::ipc::dto::RunRetryRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let run_id = request.run_id.clone();
        self.call(async move {
            let run_id = aether_core::RunId::new(run_id)
                .map_err(|error| IpcError::internal(format!("run_id 非法：{error}")))?;
            let ack = manager
                .retry_run(&run_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": ack.session_id.as_str(),
                "run_id": ack.run_id.as_str(),
                "input_message_id": ack.input_message_id.as_str(),
                "queued": ack.queued,
            }))
        })
    }

    fn messages_page(&self, request: &MessagesPageRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let session_id = request.session_id.clone();
        let last_seq = request.last_seq;
        let limit = request.limit.unwrap_or(500) as usize;
        let gap_limit = self.gap_limit;
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let max_seq = reads
                .max_seq(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("events.max(seq) 读取失败：{error}")))?;
            let events = match last_seq {
                Some(last_seq) => {
                    if let Some(max_seq) = max_seq {
                        let gap = max_seq.saturating_sub(last_seq);
                        if gap > gap_limit {
                            return Err(IpcError::new(
                                IpcErrorCode::ReadbackGapTooLarge,
                                format!(
                                    "补读缺口过大（readback_gap_too_large）：{gap} > {gap_limit}，拒绝自动补发；请重开会话"
                                ),
                            ));
                        }
                    }
                    reads
                        .events_page(&session_id, Some(last_seq), limit)
                        .await
                        .map_err(|error| {
                            IpcError::internal(format!("补读分页失败：{error}"))
                        })?
                }
                None => reads
                    .events_latest(&session_id, limit)
                    .await
                    .map_err(|error| IpcError::internal(format!("最近事件分页失败：{error}")))?,
            };
            // 最近一页附带消息历史（工作台基线；`messages` 表尾部 limit 条，升序；
            // 无消息为 `Some([])`；补读热路径不读消息表，字段省略）。
            let messages = if last_seq.is_none() {
                Some(
                    reads
                        .messages_latest(&session_id, limit)
                        .await
                        .map_err(|error| IpcError::internal(format!("消息历史分页失败：{error}")))?,
                )
            } else {
                None
            };
            let complete = match (last_seq, max_seq) {
                (Some(last_seq), Some(max_seq)) => {
                    // `last_seq >= max_seq` 视为已到最新（空 events + complete=true）。
                    last_seq >= max_seq || events.last().map(|event| event.seq) == Some(max_seq)
                }
                (Some(_), None) => true,
                // 最近一页即会话尾部（更早历史属于「基线」，不构成后续缺口）。
                (None, _) => true,
            };
            serde_json::to_value(MessagesPageResponse {
                session_id: session_id.as_str().to_owned(),
                last_seq,
                max_seq,
                events,
                messages,
                complete,
            })
            .map_err(|error| IpcError::internal(format!("消息分页序列化失败：{error}")))
        })
    }

    /// M3-03 `permissions_pending`：待审批清单（D9；可按会话过滤）。
    ///
    /// 响应为数组（按 `requested_at` 升序，即审批队列顺序），条目字段见 M3-03 证据
    /// （`id` / `request_id` / `session_id` / `resource` / `action` / `target` /
    /// `canonical_target` / `requested_at` / `timeout_ms`）。UI 以 `target` 与
    /// `canonical_target` 并排对照展示（D9 评审 #10 防视觉欺骗）。
    fn permissions_pending(&self, request: &PermissionsPendingRequest) -> Result<Value, IpcError> {
        let service = self.permissions_required()?;
        let filter = match &request.session_id {
            Some(session_id) => Some(
                SessionId::new(session_id.clone())
                    .map_err(|error| IpcError::invalid_format("session_id", error.to_string()))?,
            ),
            None => None,
        };
        let timeout_ms = service.config().ask_timeout_ms;
        let items: Vec<Value> = service
            .pending_list(filter.as_ref())
            .iter()
            .map(|ticket| {
                json!({
                    "id": ticket.id,
                    "request_id": ticket.request_id,
                    "session_id": ticket.session_id,
                    "resource": ticket.resource,
                    "action": ticket.action,
                    "target": ticket.target,
                    "canonical_target": ticket.canonical_target,
                    "requested_at": ticket.requested_at,
                    "timeout_ms": timeout_ms,
                })
            })
            .collect();
        Ok(Value::Array(items))
    }

    /// M3-03 `permission_resolve`：用户决议（D9：`once` / `session` 授权；`deny` 拒绝）。
    ///
    /// DTO 决策 → 控制层（`once` → allow+once；`session` → allow+session；`deny` → deny）。
    /// 非待审批（已决议/已超时/不存在）→ `invalid_value` + 稳定业务码
    /// `permission_not_pending`（不新增 IPC 错误码枚举，口径同 `session_busy`）。
    fn permission_resolve(&self, request: &PermissionResolveRequest) -> Result<Value, IpcError> {
        let service = self.permissions_required()?.clone();
        let request_id = request.request_id.clone();
        let (decision, scope) = match request.decision {
            crate::ipc::dto::PermissionDecision::Once => {
                (PermissionDecision::Allow, Some(PermissionScope::Once))
            }
            crate::ipc::dto::PermissionDecision::Session => {
                (PermissionDecision::Allow, Some(PermissionScope::Session))
            }
            crate::ipc::dto::PermissionDecision::Deny => (PermissionDecision::Deny, None),
        };
        self.call(async move {
            let ticket = service
                .resolve(&request_id, decision, scope)
                .await
                .map_err(map_permission_error)?;
            Ok(json!({
                "request_id": request_id,
                "decision": decision.as_str(),
                "scope": scope.map(|scope| scope.as_str()),
                "ticket_id": ticket.id,
            }))
        })
    }

    // ===== 装饰器透传（M3-04 修正）：外层未覆写的方法必须显式委派给内层，
    // 否则会命中 trait 默认 `not_implemented`，令内层已实现命令在生产链路上不可达
    // （设置、运行时控制、工作区、诊断导出、备份族）。=====

    fn settings_get(
        &self,
        request: &crate::ipc::dto::SettingsGetRequest,
    ) -> Result<Value, IpcError> {
        self.inner.settings_get(request)
    }

    fn settings_set(
        &self,
        request: &crate::ipc::dto::SettingsSetRequest,
    ) -> Result<Value, IpcError> {
        self.inner.settings_set(request)
    }

    fn runtime_retry(
        &self,
        request: &crate::ipc::dto::RuntimeRetryRequest,
    ) -> Result<Value, IpcError> {
        self.inner.runtime_retry(request)
    }

    fn runtime_enable(
        &self,
        request: &crate::ipc::dto::RuntimeEnableRequest,
    ) -> Result<Value, IpcError> {
        self.inner.runtime_enable(request)
    }

    fn export_diagnostics(
        &self,
        request: &crate::ipc::dto::ExportDiagnosticsRequest,
        canonical_target_dir: &std::path::Path,
    ) -> Result<Value, IpcError> {
        self.inner.export_diagnostics(request, canonical_target_dir)
    }

    fn backup_create(
        &self,
        request: &crate::ipc::dto::BackupCreateRequest,
        canonical_target_dir: Option<&std::path::Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_create(request, canonical_target_dir)
    }

    fn backup_list(&self) -> Result<Value, IpcError> {
        self.inner.backup_list()
    }

    fn backup_restore(
        &self,
        request: &crate::ipc::dto::BackupRestoreRequest,
        canonical_external_path: Option<&std::path::Path>,
    ) -> Result<Value, IpcError> {
        self.inner.backup_restore(request, canonical_external_path)
    }
}

fn parse_session_id(value: &str) -> Result<SessionId, IpcError> {
    SessionId::new(value).map_err(|error| IpcError::internal(format!("session_id 非法：{error}")))
}

/// 工作区绑定互斥锁（中毒恢复语义与其他模块一致：不因单次 panic 永久拒绝绑定）。
fn lock_workspace(
    slot: &Arc<Mutex<Option<WorkspaceBinding>>>,
) -> std::sync::MutexGuard<'_, Option<WorkspaceBinding>> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 核心会话状态 → DTO（`SessionSummary` 响应字段；取值一一对应）。
fn session_status_to_dto(status: SessionStatus) -> crate::ipc::dto::SessionStatus {
    match status {
        SessionStatus::Creating => crate::ipc::dto::SessionStatus::Creating,
        SessionStatus::Idle => crate::ipc::dto::SessionStatus::Idle,
        SessionStatus::Running => crate::ipc::dto::SessionStatus::Running,
        SessionStatus::Paused => crate::ipc::dto::SessionStatus::Paused,
        SessionStatus::WaitingPermission => crate::ipc::dto::SessionStatus::WaitingPermission,
        SessionStatus::Completed => crate::ipc::dto::SessionStatus::Completed,
        SessionStatus::Failed => crate::ipc::dto::SessionStatus::Failed,
        SessionStatus::Cancelled => crate::ipc::dto::SessionStatus::Cancelled,
    }
}

/// 会话不存在（引用命令面：`artifacts_list` / `artifact_add`）。
fn session_not_found(session_id: &SessionId) -> IpcError {
    IpcError::invalid_value(format!(
        "session_id 不存在（{}）；引用操作仅对既有会话可用",
        session_id.as_str()
    ))
}

/// 引用条目响应形状（ADR-010 附录 B.1：`{ id, path, kind, size_bytes, created_at }`）。
fn artifact_json(artifact: &ArtifactRecord) -> Value {
    json!({
        "id": artifact.id,
        "path": artifact.path,
        "kind": artifact.kind,
        "size_bytes": artifact.size_bytes,
        "created_at": artifact.created_at,
    })
}

/// DTO 会话状态 → 核心状态（取值一一对应；DTO 校验已限定枚举）。
fn map_session_status(status: crate::ipc::dto::SessionStatus) -> SessionStatus {
    match status {
        crate::ipc::dto::SessionStatus::Creating => SessionStatus::Creating,
        crate::ipc::dto::SessionStatus::Idle => SessionStatus::Idle,
        crate::ipc::dto::SessionStatus::Running => SessionStatus::Running,
        crate::ipc::dto::SessionStatus::Paused => SessionStatus::Paused,
        crate::ipc::dto::SessionStatus::WaitingPermission => SessionStatus::WaitingPermission,
        crate::ipc::dto::SessionStatus::Completed => SessionStatus::Completed,
        crate::ipc::dto::SessionStatus::Failed => SessionStatus::Failed,
        crate::ipc::dto::SessionStatus::Cancelled => SessionStatus::Cancelled,
    }
}

/// 生命周期错误 → IPC 结构化错误（状态冲突用 `invalid_value`；内部错误用 `internal`）。
///
/// `session_busy` / `storage_backpressure` / `persist_degraded` 的专用 IPC 错误码
/// 未在本任务登记（避免超出 M3-02 范围新增错误码面）；消息保留稳定业务码，
/// 供 UI 展示与后续 M3-06 降级 UX 消费。
pub fn map_lifecycle_error(error: LifecycleError) -> IpcError {
    match error {
        LifecycleError::Internal { .. } => IpcError::internal(error.to_string()),
        LifecycleError::Storage { .. } | LifecycleError::Pipeline { .. } => {
            IpcError::internal(error.to_string())
        }
        LifecycleError::SessionNotFound { .. }
        | LifecycleError::SessionBusy { .. }
        | LifecycleError::SessionClosed { .. }
        | LifecycleError::InvalidTransition { .. }
        | LifecycleError::RunNotFound { .. }
        | LifecycleError::RunNotRetryable { .. }
        | LifecycleError::PersistDegraded { .. }
        | LifecycleError::StorageBackpressure { .. }
        | LifecycleError::AdapterIsolated { .. } => IpcError::invalid_value(error.to_string()),
    }
}

/// 权限服务错误 → IPC 结构化错误（M3-03；不新增 IPC 错误码枚举）。
///
/// 业务类错误（不在待审批队列 / 非法资源 / 会话不存在）用 `invalid_value` 并在消息中
/// 携带稳定业务码（`permission_not_pending` / `invalid_permission_resource` /
/// `session_not_found`），口径同 `session_busy`：UI 按 `message` 展示，行为判定
/// 不依赖文案。存储/管线/内部错误用 `internal`。
pub fn map_permission_error(error: PermissionError) -> IpcError {
    match &error {
        PermissionError::NotPending { .. } => {
            IpcError::invalid_value(format!("{error}（permission_not_pending）"))
        }
        PermissionError::InvalidResource { .. } => {
            IpcError::invalid_value(format!("{error}（invalid_permission_resource）"))
        }
        PermissionError::SessionNotFound { .. } => {
            IpcError::invalid_value(format!("{error}（session_not_found）"))
        }
        PermissionError::Storage { .. }
        | PermissionError::Pipeline { .. }
        | PermissionError::Internal { .. } => IpcError::internal(error.to_string()),
    }
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn lifecycle_errors_map_to_structured_ipc_codes() {
        let busy = map_lifecycle_error(LifecycleError::SessionBusy {
            session_id: SessionId::new("01J0000000000000000000000S").unwrap(),
        });
        assert_eq!(busy.code, IpcErrorCode::InvalidValue);
        assert!(busy.message.contains("session_busy"));

        let internal = map_lifecycle_error(LifecycleError::Internal {
            reason: "boom".to_owned(),
        });
        assert_eq!(internal.code, IpcErrorCode::Internal);
    }

    #[test]
    fn session_status_mapping_is_total() {
        assert_eq!(
            map_session_status(crate::ipc::dto::SessionStatus::WaitingPermission),
            SessionStatus::WaitingPermission
        );
        assert_eq!(
            map_session_status(crate::ipc::dto::SessionStatus::Cancelled),
            SessionStatus::Cancelled
        );
    }

    #[test]
    fn readback_gap_code_is_stable() {
        assert_eq!(
            IpcErrorCode::ReadbackGapTooLarge.as_str(),
            "readback_gap_too_large"
        );
        assert_eq!(
            IpcErrorCode::ReadbackGapTooLarge.as_str(),
            aether_control::PipelineError::ReadbackGapTooLarge {
                gap: 10_001,
                limit: 10_000
            }
            .code(),
            "与核心管线错误码一致（同码透传）"
        );
    }
}
