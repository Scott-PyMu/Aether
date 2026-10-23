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
    compose_memory_injection, compose_system_prompt, config_with_session_baseline,
    session_baseline_from_config, LifecycleError, MemoryInjection, PermissionError,
    PermissionService, SessionCreateOptions, SessionManager, MEMORY_INJECTION_HEADER,
    READBACK_MAX_GAP,
};
use aether_core::{
    EventEnvelope, Message, PermissionDecision, PermissionScope, Preset, PresetId, Runtime,
    RuntimeId, RuntimeStatus, SessionId, SessionStatus, Workspace, WorkspaceId,
    PAUSE_RESUME_CAPABILITY, THINKING_DEPTH_CAPABILITY, THINKING_DEPTH_DEFAULT,
};
use aether_security::{normalize_rule_target, PermissionResource, RuleError, RuleTargetKind};
use aether_store::{
    ArtifactRecord, AuditLogRecord, PermissionRuleRecord, ReadPool, SessionQuery, StoreCommand,
    StoreOutcome, WriteQueue,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    ArtifactAddRequest, ArtifactRemoveRequest, ArtifactsListRequest, MessagesPageRequest,
    PermissionResolveRequest, PermissionsPendingRequest, PresetAddRequest, PresetDeleteRequest,
    PresetUpdateRequest, PresetsListRequest, ProviderCreateRequest, ProviderDeleteRequest,
    ProviderModelAddRequest, ProviderModelToggleRequest, ProviderModelsFetchRequest,
    ProviderTestRequest, ProviderToggleRequest, ProviderUpdateRequest, ProvidersListRequest,
    RuleAddRequest, RuleDeleteRequest, RuleToggleRequest, RuleUpdateRequest, SessionCreateRequest,
    SessionDeleteRequest, SessionIdRequest, SessionListRequest, SessionPermissionModeSetRequest,
    SessionRenameRequest, SessionSearchRequest, SessionSendRequest, SessionWorkspaceAssignRequest,
    WorkspaceCreateRequest, WorkspaceDeleteRequest, WorkspaceUpdateRequest, WorkspacesListRequest,
};
use crate::ipc::error::{IpcError, IpcErrorCode};
use crate::ipc::path::ArtifactPath;
use crate::json_payload::JsonPayload;

use crate::adapter_executor::AdapterRunExecutor;
use crate::preset_control::PresetControl;
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
    /// ADR-028 v0.9：会话级供应商回显（`sessions.config.aether.provider_id`；未选省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// ADR-028 v0.9：供应商名称回显（供应商已删除时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// ADR-028 v0.9：供应商状态（`active|invalid|missing`；`invalid` = 停用/删除，
    /// UI 只读并禁用所有补选）。
    pub provider_state: String,
    /// ADR-028 v0.9：模型状态（`active|invalid|missing`；provider 空时 model 在任一
    /// 启用供应商中**存在且启用** → active，否则 invalid）。
    pub model_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_depth: Option<u8>,
    /// 会话级权限模式（M6-01/ADR-014 决策 3；`SessionSummary` 回显档位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// 预设生效来源（M7-09/ADR-026 §2.2-4：`sessions.preset_id`；未选预设省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// 预设名称回显（来源已删除 → 名称缺失，`preset_id` 亦已由 `ON DELETE SET NULL` 置空）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_name: Option<String>,
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
            provider_id: session
                .config
                .get("aether")
                .and_then(|value| value.get("provider_id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            provider_name: None,
            provider_state: "missing".to_owned(),
            model_state: "missing".to_owned(),
            thinking_depth: Some(session.thinking_depth),
            permission_mode: Some(session.permission_mode.as_str().to_owned()),
            preset_id: session.preset_id.as_ref().map(|id| id.as_str().to_owned()),
            preset_name: None,
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

    /// 回显预设生效来源（M7-09/ADR-026 §2.2-4：`preset_id` 已在 `from_session` 落位，
    /// 此处补齐 `preset_name`；无预设行 → 两者均省略）。
    #[must_use]
    pub fn with_preset(mut self, preset: Option<&Preset>) -> Self {
        match preset {
            Some(preset) => {
                self.preset_id = Some(preset.id.as_str().to_owned());
                self.preset_name = Some(preset.name.clone());
            }
            None => {
                self.preset_id = None;
                self.preset_name = None;
            }
        }
        self
    }

    /// ADR-028 v0.9：按当前供应商/模型目录回显 `provider_state` / `model_state`。
    #[must_use]
    pub fn with_provider_selection(
        mut self,
        providers: &[aether_store::ProviderRecord],
        models: &[aether_store::ProviderModelRecord],
    ) -> Self {
        let (name, provider_state, model_state) = provider_selection_state(
            self.provider_id.as_deref(),
            self.model.as_deref(),
            providers,
            models,
        );
        self.provider_name = name;
        self.provider_state = provider_state.to_owned();
        self.model_state = model_state.to_owned();
        self
    }
}

/// 供应商/模型状态判定（ADR-028 v0.9 §2.1-4 回显状态）。
///
/// - `provider_state`：未选 → `missing`；存在且启用 → `active`；停用/删除 → `invalid`；
/// - `model_state`：provider 已选 → 仅该 provider 的启用模型算 `active`；provider 空 →
///   任一启用 provider 中**存在且启用** → `active`，否则 `invalid`；无 model → `missing`；
/// - provider 失效优先：`provider_state=invalid` 时 `model_state=invalid`（UI 只读，
///   禁用所有补选）。
fn provider_selection_state(
    provider_id: Option<&str>,
    model: Option<&str>,
    providers: &[aether_store::ProviderRecord],
    models: &[aether_store::ProviderModelRecord],
) -> (Option<String>, &'static str, &'static str) {
    let model = model.filter(|value| !value.is_empty());
    let Some(provider_id) = provider_id.filter(|value| !value.is_empty()) else {
        let model_state = match model {
            None => "missing",
            Some(model) => {
                if providers.iter().any(|provider| {
                    provider.enabled
                        && models.iter().any(|candidate| {
                            candidate.provider_id == provider.id
                                && candidate.model_id == model
                                && candidate.enabled
                        })
                }) {
                    "active"
                } else {
                    "invalid"
                }
            }
        };
        return (None, "missing", model_state);
    };
    let Some(provider) = providers.iter().find(|item| item.id == provider_id) else {
        return (None, "invalid", "invalid");
    };
    if !provider.enabled {
        return (Some(provider.name.clone()), "invalid", "invalid");
    }
    let model_state = match model {
        None => "missing",
        Some(model) => {
            if models.iter().any(|candidate| {
                candidate.provider_id == provider_id
                    && candidate.model_id == model
                    && candidate.enabled
            }) {
                "active"
            } else {
                "invalid"
            }
        }
    };
    (Some(provider.name.clone()), "active", model_state)
}

/// 能力门同步判定（ADR-010 决策 2）：runtime ready 且未声明 `thinking_depth` 能力时
/// 返回 `warnings[0].code="thinking_depth_unsupported"`（非阻断；延迟判定路径——
/// runtime 未 ready——不产生响应警告，以 `SessionSummary.thinking_depth` 回显为准）。
pub fn thinking_depth_warning(
    runtime_ready: bool,
    capabilities: &[String],
    runtime_id: &str,
) -> Option<Vec<SessionWarning>> {
    if runtime_ready
        && !capabilities
            .iter()
            .any(|item| item == THINKING_DEPTH_CAPABILITY)
    {
        return Some(vec![SessionWarning {
            code: "thinking_depth_unsupported".to_owned(),
            field: "thinking_depth".to_owned(),
            runtime_id: runtime_id.to_owned(),
            message: "当前运行时不支持思考深度，已按默认档位运行".to_owned(),
        }]);
    }
    None
}

/// 模型硬条件（M7-02/ADR-022 §2.1#2②）：存在**启用**供应商提供该模型（供应商行与
/// 模型行均 `enabled`）。M7-06 完成消费映射物化前的既有数据口径判定。
async fn provider_provides_model(reads: &ReadPool, model: &str) -> Result<bool, IpcError> {
    let providers = reads
        .providers()
        .await
        .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
    if !providers.iter().any(|provider| provider.enabled) {
        return Ok(false);
    }
    let models = reads
        .provider_models()
        .await
        .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
    Ok(models.iter().any(|candidate| {
        candidate.enabled
            && candidate.model_id == model
            && providers
                .iter()
                .any(|provider| provider.enabled && provider.id == candidate.provider_id)
    }))
}

/// 候选摘要（`no_candidate_runtime` 消息「可用运行时清单一旁展示」文本）。
fn candidate_summary(candidates: &[aether_control::RouteCandidate]) -> String {
    if candidates.is_empty() {
        return "（无）".to_owned();
    }
    candidates
        .iter()
        .map(|candidate| {
            format!(
                "{}[{}，enabled={}]",
                candidate.id,
                candidate.status.as_str(),
                candidate.enabled
            )
        })
        .collect::<Vec<_>>()
        .join("，")
}

/// 工作区绑定（M3-08；设计 D14/ADR-004 决策 3；M7-08/R15 语义收敛）。
///
/// 语义：`root_path` 为 canonical 路径；`workspace_set` 后**新会话**按该工作区注入
/// 记忆文件并把**缺省**权限基准目录换绑到同一根（`PermissionService` 策略引擎重建）；
/// **既有会话保持创建时基准**（M7-08/R15：`sessions.config.aether.workspace_baseline_root`
/// 快照，不因换根迁移；见 ADR-014 附录 B 不变量 11）。
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
    /// Agent 预设命令执行体（M7-09；`None` = 未接线 → 预设命令回 `core_not_ready`）。
    presets: Option<PresetControl>,
    /// ADR-028 v0.9：供应商凭据预检（broker 可用性 + key 可读；读取后立即丢弃）。
    provider_env: Option<Arc<crate::provider_env::ProviderEnvHook>>,
    /// 当前工作区绑定（M3-08；`None` = 未绑定）。
    workspace: Arc<Mutex<Option<WorkspaceBinding>>>,
    /// 数据目录（M7-08 `workspaces_list` 合成组「默认（数据目录）」的根路径展示）。
    data_dir: Option<std::path::PathBuf>,
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
            presets: None,
            provider_env: None,
            workspace: Arc::new(Mutex::new(None)),
            data_dir: None,
            handle,
            timeout: SESSION_COMMAND_TIMEOUT,
            gap_limit: READBACK_MAX_GAP,
        }
    }

    /// 注入数据目录（M7-08：合成组「默认（数据目录）」的根路径展示；`None` = 不展示）。
    #[must_use]
    pub fn with_data_dir(mut self, data_dir: impl Into<std::path::PathBuf>) -> Self {
        self.data_dir = Some(data_dir.into());
        self
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

    /// 注入 Agent 预设命令执行体（M7-09：`presets_list` / `preset_*` 四命令）。
    #[must_use]
    pub fn with_presets(mut self, presets: PresetControl) -> Self {
        self.presets = Some(presets);
        self
    }

    /// 注入供应商凭据预检（ADR-028 v0.9：`session_create` fail-fast + 旧会话补选）。
    #[must_use]
    pub fn with_provider_env(
        mut self,
        provider_env: Arc<crate::provider_env::ProviderEnvHook>,
    ) -> Self {
        self.provider_env = Some(provider_env);
        self
    }

    /// 预设命令执行体（未接线 → 预设命令回 `core_not_ready`）。
    fn presets_required(&self) -> Result<&PresetControl, IpcError> {
        self.presets.as_ref().ok_or_else(|| {
            IpcError::core_not_ready(
                "预设后端未接线：presets_list / preset_* 不可用（启动序列未完成）",
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
        let reads = self.reads.clone();
        self.call(async move {
            // M7-01/ADR-015 决策 6：`enabled` 取持久化偏好（`runtimes.enabled`），
            // `active_sessions` = 在途计数（与 `runtime_disable` 确认门同源读路径）。
            // 无存储接线的测试构造回退构建期缺省（`RuntimeManifest.enabled`；计数 0）。
            let preferences = match &reads {
                Some(reads) => reads.runtime_preferences().await.map_err(|error| {
                    IpcError::internal(format!("运行时启停偏好读取失败：{error}"))
                })?,
                None => std::collections::BTreeMap::new(),
            };
            let active_work = match &reads {
                Some(reads) => reads.active_work_counts().await.map_err(|error| {
                    IpcError::internal(format!("在途会话计数读取失败：{error}"))
                })?,
                None => std::collections::BTreeMap::new(),
            };
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
                            let mut capabilities = hello.runtime.capabilities;
                            for capability in aether_core::static_capabilities(&id) {
                                if !capabilities.iter().any(|item| item == capability) {
                                    capabilities.push((*capability).to_owned());
                                }
                            }
                            capabilities
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            aether_core::static_capabilities(&id)
                                .iter()
                                .map(|capability| (*capability).to_owned())
                                .collect()
                        }
                    },
                    None => aether_core::static_capabilities(&id)
                        .iter()
                        .map(|capability| (*capability).to_owned())
                        .collect(),
                };
                let enabled = preferences.get(&id).copied().unwrap_or(manifest.enabled);
                let active_sessions = active_work.get(&id).copied().unwrap_or(0);
                runtimes.push(json!({
                    "id": manifest.id,
                    "name": manifest.name,
                    "kind": manifest.kind,
                    "version": manifest.version,
                    "protocol": manifest.protocol,
                    "capabilities": capabilities,
                    "enabled": enabled,
                    "status": status.as_str(),
                    "status_reason": reason.map(|reason| reason.as_str()),
                    "active_sessions": active_sessions,
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
            // M7-09/ADR-026 §2.2-4：预设名称回显（按 preset_id 去重读取；预设已删除的
            // 会话 `preset_id` 已被 `ON DELETE SET NULL` 置空，不出现悬空 id）。
            let mut presets: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for session in &sessions {
                let Some(preset_id) = &session.preset_id else {
                    continue;
                };
                let key = preset_id.as_str().to_owned();
                if presets.contains_key(&key) {
                    continue;
                }
                if let Some(preset) = reads
                    .preset(preset_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("预设读取失败：{error}")))?
                {
                    presets.insert(key, preset.name);
                }
            }
            let providers = reads
                .providers()
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
            let provider_models = reads
                .provider_models()
                .await
                .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
            let summaries = sessions
                .iter()
                .map(|session| {
                    let root = session
                        .workspace_id
                        .as_ref()
                        .and_then(|id| roots.get(id.as_str()).cloned().flatten());
                    let mut summary = SessionSummary::from_session(session, root)?;
                    if let Some(name) = session
                        .preset_id
                        .as_ref()
                        .and_then(|id| presets.get(id.as_str()))
                    {
                        summary.preset_name = Some(name.clone());
                    }
                    Ok(summary.with_provider_selection(&providers, &provider_models))
                })
                .collect::<Result<Vec<_>, IpcError>>()?;
            serde_json::to_value(summaries)
                .map_err(|error| IpcError::internal(format!("会话列表序列化失败：{error}")))
        })
    }

    /// M7-07/ADR-025 §2.1-2：会话全文检索（标题 + 消息正文；`snippet` + 命中来源）。
    fn session_search(&self, request: &SessionSearchRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let phrase = request.phrase();
        let limit = request.limit_value()?;
        let offset = request.offset.unwrap_or(0);
        self.call(async move {
            let hits = reads
                .search_sessions(&phrase, limit, offset)
                .await
                .map_err(|error| IpcError::internal(format!("会话检索失败：{error}")))?;
            let providers = reads
                .providers()
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
            let provider_models = reads
                .provider_models()
                .await
                .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
            let mut items = Vec::with_capacity(hits.len());
            for hit in hits {
                let summary = SessionSummary::from_session(&hit.session, None)?
                    .with_provider_selection(&providers, &provider_models);
                let mut value = serde_json::to_value(summary)
                    .map_err(|error| IpcError::internal(format!("会话检索序列化失败：{error}")))?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("snippet".to_owned(), Value::from(hit.snippet));
                    object.insert("source".to_owned(), Value::from(hit.source));
                }
                items.push(value);
            }
            Ok(Value::Array(items))
        })
    }

    /// M7-07/ADR-025 §2.1-3：会话改名（单写队列；`session.updated` + 审计）。
    fn session_rename(&self, request: &SessionRenameRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let session_id = parse_session_id(&request.session_id)?;
        let title = request.title.clone();
        self.call(async move {
            let session = manager
                .rename_session(&session_id, &title)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": session.id.as_str(),
                "title": session.title,
                "updated_at": session.updated_at,
            }))
        })
    }

    /// M7-07/ADR-025 §2.1-4：硬删除会话（在途/待审批 → `session_busy`；
    /// `events` 不级联，R7）。
    fn session_delete(&self, request: &SessionDeleteRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let session_id = parse_session_id(&request.session_id)?;
        self.call(async move {
            manager
                .delete_session(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": session_id.as_str(),
                "deleted": true,
            }))
        })
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let supervisor = self.supervisor.clone();
        let reads = self.reads.clone();
        let permissions = self.permissions.clone();
        let write = self.write.clone();
        let binding = lock_workspace(&self.workspace).clone();
        let requested_runtime_id = request.runtime_id.clone();
        let title = request.title.clone();
        let workspace_id = request.workspace_id.clone();
        let model = request.model.clone();
        let provider_id_request = request.provider_id.clone();
        let provider_env = self.provider_env.clone();
        let thinking_depth_request = request.thinking_depth_value()?;
        let permission_mode = request.permission_mode_value()?.unwrap_or_default();
        let preset_id = request.preset_id.clone();
        self.call(async move {
            let supervisor = supervisor.ok_or_else(|| {
                IpcError::core_not_ready("监督器未接线：无法解析 runtime_id（启动序列未完成）")
            })?;
            // M7-02/ADR-022 §2.1：`runtime_id` 缺省 = 自动路由（固定优先级）；显式 =
            // 手动指定（优先，不参与打分；仍须通过可用性校验）。候选快照每次现读
            // （无粘性缓存）——与热插拔联动：禁用即排除、恢复即重新纳入。
            let (runtime_id, route) = match &requested_runtime_id {
                Some(runtime_id) => (runtime_id.clone(), None),
                None => {
                    let preferences = match &reads {
                        Some(reads) => reads.runtime_preferences().await.map_err(|error| {
                            IpcError::internal(format!("运行时启停偏好读取失败：{error}"))
                        })?,
                        None => std::collections::BTreeMap::new(),
                    };
                    let mut candidates = Vec::new();
                    for id in supervisor.runtime_ids() {
                        let Some(runtime) = supervisor.get(&id) else {
                            continue;
                        };
                        let (status, _reason) = runtime.summary_snapshot();
                        let enabled = preferences
                            .get(&id)
                            .copied()
                            .unwrap_or(runtime.manifest().enabled);
                        candidates.push(aether_control::RouteCandidate {
                            id,
                            enabled,
                            status,
                            // DSH 直通由 M7-06 实测登记投影映射后置位（当前恒 false）。
                            model_passthrough: false,
                        });
                    }
                    // 模型硬条件：存在启用 provider 提供该模型（ADR-022 §2.1#2②；
                    // M7-06 物化消费前，该判定即「模型可路由」的既有数据口径）。
                    let model_available = match (&model, &reads) {
                        (Some(model), Some(reads)) => {
                            provider_provides_model(reads, model).await?
                        }
                        _ => false,
                    };
                    let decision = aether_control::route_runtime(
                        model.as_deref(),
                        model_available,
                        &candidates,
                    )
                    .map_err(|failure| {
                        IpcError::no_candidate_runtime(
                            &failure.reason(),
                            &candidate_summary(&candidates),
                        )
                    })?;
                    (decision.runtime_id.clone(), Some(decision))
                }
            };
            let runtime = {
                let runtime = supervisor.get(&runtime_id).ok_or_else(|| {
                    IpcError::invalid_enum(
                        "runtime_id",
                        format!("runtime_id {runtime_id:?} 不在注册表（监督器白名单）"),
                    )
                })?;
                let (status, reason) = runtime.summary_snapshot();
                // M7-02/ADR-022 §2.1#5：手动/自动选中均须通过可用性校验——偏好
                // `enabled=0` 或监督状态 `disabled` → `invalid_value`（提示启用或改选）。
                let enabled = match &reads {
                    Some(reads) => reads
                        .runtime_preferences()
                        .await
                        .map_err(|error| {
                            IpcError::internal(format!("运行时启停偏好读取失败：{error}"))
                        })?
                        .get(&runtime_id)
                        .copied()
                        .unwrap_or(runtime.manifest().enabled),
                    None => runtime.manifest().enabled,
                };
                if !enabled {
                    return Err(IpcError::invalid_value(format!(
                        "runtime {runtime_id:?} 已被用户禁用（enabled=0），不可创建会话；\
                         请先启用或改选其他运行时"
                    )));
                }
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
                            let mut capabilities = hello.runtime.capabilities.clone();
                            // ADR-028 v0.9：`runtimes_list.capabilities` = hello ∪ 核心静态表。
                            for capability in aether_core::static_capabilities(&runtime_id) {
                                if !capabilities.iter().any(|item| item == capability) {
                                    capabilities.push((*capability).to_owned());
                                }
                            }
                            (hello.runtime.version, capabilities, true)
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            let mut capabilities = Vec::new();
                            for capability in aether_core::static_capabilities(&runtime_id) {
                                capabilities.push((*capability).to_owned());
                            }
                            (manifest.version.clone(), capabilities, false)
                        }
                    },
                    None => {
                        let mut capabilities = Vec::new();
                        for capability in aether_core::static_capabilities(&runtime_id) {
                            capabilities.push((*capability).to_owned());
                        }
                        (manifest.version.clone(), capabilities, false)
                    }
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
            let (workspace_id, memory_prompt) = match &resolved_workspace {
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
            // M7-09/ADR-026 §2.2-3：预设块（置前）→ 工作区记忆块（后置）合成；
            // 禁止静默覆盖（两者均完整保留）；未选预设 → 仅记忆块（与 P0 行为一致）。
            let preset = match &preset_id {
                Some(value) => {
                    let reads = reads.clone().ok_or_else(|| {
                        IpcError::core_not_ready(
                            "会话后端未接线：preset_id 存在性校验需要读连接池",
                        )
                    })?;
                    let id = PresetId::new(value.clone()).map_err(|error| {
                        IpcError::internal(format!("preset_id 非法：{error}"))
                    })?;
                    Some(
                        reads
                            .preset(&id)
                            .await
                            .map_err(|error| IpcError::internal(format!("预设读取失败：{error}")))?
                            .ok_or_else(|| IpcError::preset_not_found(value))?,
                    )
                }
                None => None,
            };
            let system_prompt = compose_system_prompt(preset.as_ref(), memory_prompt.as_deref());
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
            // ADR-028 v0.9：`multi_provider` 为**核心静态能力**，不以 hello/runtime ready
            // 为判定门禁（ADR-028 §3-1）。
            let supports_multi_provider = aether_core::supports_multi_provider(runtime.id.as_str());
            let provider_id = resolve_session_provider(
                reads.as_ref(),
                runtime.id.as_str(),
                provider_id_request.as_deref(),
                model.as_deref(),
                supports_multi_provider,
                provider_env.as_deref(),
            )
            .await?;
            let session = manager
                .create_session_with_options(
                    runtime,
                    &title,
                    SessionCreateOptions {
                        workspace_id,
                        model,
                        system_prompt,
                        preset_id: preset.as_ref().map(|preset| preset.id.clone()),
                        thinking_depth: effective_depth,
                        permission_mode,
                        provider_id,
                        // M7-02/ADR-022：自动路由决策随创建入控制层（审计
                        // `route.decided` + `session.created.routed=true`）；手动为 `None`。
                        route,
                    },
                )
                .await
                .map_err(map_lifecycle_error)?;
            // M7-08/R15（ADR-014 附录 B 不变量 11）：**权限基准按会话创建固定**——
            // 登记创建时工作区根（`workspace_id IS NULL` → 数据目录）到权限服务，并把
            // 同一快照持久化到 `sessions.config.aether.workspace_baseline_root`
            // （跨重启不变；`workspace_set` 换根与改派均不重算）。
            let baseline_root = resolved_workspace
                .as_ref()
                .map(|workspace| std::path::PathBuf::from(&workspace.root_path));
            persist_session_baseline(
                write.as_ref(),
                permissions.as_ref(),
                &session,
                baseline_root.as_deref(),
            )
            .await;
            // ADR-010 附录 B.4：响应定型为 `SessionSummary`（`workspace_root` 取自
            // 本会话解析出的工作区；未绑定省略；`thinking_depth` 回显生效值）；
            // M7-09/ADR-026 §2.2-4：`preset_id`/`preset_name` 回显生效来源。
            let workspace_root = resolved_workspace
                .as_ref()
                .map(|workspace| workspace.root_path.clone());
            let (providers, provider_models) = match &reads {
                Some(reads) => (
                    reads.providers().await.map_err(|error| {
                        IpcError::internal(format!("供应商读取失败：{error}"))
                    })?,
                    reads.provider_models().await.map_err(|error| {
                        IpcError::internal(format!("供应商模型读取失败：{error}"))
                    })?,
                ),
                None => (Vec::new(), Vec::new()),
            };
            let summary = SessionSummary::from_session(&session, workspace_root)?
                .with_preset(preset.as_ref())
                .with_provider_selection(&providers, &provider_models)
                .with_warnings(warnings);
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

    // ===== M7-08/ADR-026 §2.1：工作区管理与多工作区（五命令）=====

    /// M7-08 `workspaces_list`（ADR-026 §2.1-1）：工作区清单 + 会话计数
    /// （`active`/`total`）。未分配会话（`workspace_id IS NULL`）以合成组
    /// 「默认（数据目录）」呈现，**不建 `workspaces` 行**。
    fn workspaces_list(&self, _request: &WorkspacesListRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let data_dir = self.data_dir.clone();
        self.call(async move {
            let workspaces = reads
                .workspaces()
                .await
                .map_err(|error| IpcError::internal(format!("工作区清单读取失败：{error}")))?;
            let counts = reads
                .workspace_session_counts()
                .await
                .map_err(|error| IpcError::internal(format!("工作区会话计数读取失败：{error}")))?;
            Ok(json!({
                "workspaces": workspaces
                    .iter()
                    .map(|workspace| {
                        let count = counts
                            .by_workspace
                            .get(workspace.id.as_str())
                            .copied()
                            .unwrap_or_default();
                        workspace_json(workspace, count)
                    })
                    .collect::<Vec<_>>(),
                "unassigned": {
                    "name": "默认（数据目录）",
                    "root_path": data_dir
                        .as_ref()
                        .map(|path| path.to_string_lossy().to_string()),
                    "active_sessions": counts.unassigned.active,
                    "total_sessions": counts.unassigned.total,
                },
            }))
        })
    }

    /// M7-08 `workspace_create`（ADR-026 §2.1-1）：canonicalize + 存在目录 + 同步盘拒绝
    /// 已在命令层完成；本层复核**同根路径唯一性**（已存在 → `invalid_value`，提示使用
    /// 既有工作区），经单写队列落行并写审计 `workspace.created`。
    fn workspace_create(
        &self,
        request: &WorkspaceCreateRequest,
        canonical_root_path: &Path,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let name = request.name.clone();
        let root_path = canonical_root_path.to_string_lossy().to_string();
        self.call(async move {
            if let Some(existing) = reads
                .workspace_by_root(&root_path)
                .await
                .map_err(|error| IpcError::internal(format!("工作区按路径读取失败：{error}")))?
            {
                return Err(IpcError::invalid_value(format!(
                    "工作区根路径已存在（{}，workspace_id={}）；请直接在左栏树中使用既有工作区",
                    existing.root_path,
                    existing.id.as_str()
                )));
            }
            let now = now_ms();
            let workspace = Workspace {
                id: WorkspaceId::new(ulid::Ulid::new().to_string()).map_err(|error| {
                    IpcError::internal(format!("workspace_id 生成失败：{error}"))
                })?,
                name,
                root_path,
                // ADR-026 §2.1-4：`memory_files` P1 不启用（恒空数组；D14 三文件名优先级不变）。
                memory_files: Vec::new(),
                created_at: now,
                updated_at: now,
            };
            let outcome = write
                .execute(StoreCommand::UpsertWorkspace {
                    workspace: workspace.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("工作区落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(IpcError::internal("工作区落库未影响任何行（不变量破坏）"));
                }
            }
            insert_workspace_audit(
                &write,
                "workspace.created",
                json!({
                    "workspace_id": workspace.id.as_str(),
                    "name": workspace.name,
                    "root_path": workspace.root_path,
                    "memory_files_enabled": false,
                }),
                now,
            )
            .await?;
            Ok(workspace_json(
                &workspace,
                aether_store::WorkspaceSessionCount::default(),
            ))
        })
    }

    /// M7-08 `workspace_update`（ADR-026 §2.1-1）：**仅改名**；`root_path` 不可变
    /// （防权限基准漂移）。查无 → `workspace_not_found`；写审计 `workspace.updated`。
    fn workspace_update(&self, request: &WorkspaceUpdateRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let workspace_id = WorkspaceId::new(request.workspace_id.clone())
            .map_err(|error| IpcError::internal(format!("workspace_id 非法：{error}")))?;
        let name = request.name.clone();
        self.call(async move {
            let workspace = reads
                .workspace(&workspace_id)
                .await
                .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                .ok_or_else(|| IpcError::workspace_not_found(workspace_id.as_str()))?;
            let previous_name = workspace.name.clone();
            let now = now_ms();
            let outcome = write
                .execute(StoreCommand::RenameWorkspace {
                    id: workspace_id.clone(),
                    name: name.clone(),
                    updated_at: now,
                })
                .await
                .map_err(|error| IpcError::internal(format!("工作区改名落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(IpcError::workspace_not_found(workspace_id.as_str()));
                }
            }
            insert_workspace_audit(
                &write,
                "workspace.updated",
                json!({
                    "workspace_id": workspace_id.as_str(),
                    "from": previous_name,
                    "to": name,
                    "root_path_immutable": workspace.root_path,
                }),
                now,
            )
            .await?;
            let counts = reads
                .workspace_session_counts()
                .await
                .map_err(|error| IpcError::internal(format!("工作区会话计数读取失败：{error}")))?;
            let count = counts
                .by_workspace
                .get(workspace_id.as_str())
                .copied()
                .unwrap_or_default();
            let mut updated = workspace;
            updated.name = name;
            updated.updated_at = now;
            Ok(workspace_json(&updated, count))
        })
    }

    /// M7-08 `workspace_delete`（ADR-026 §2.1-1）：存在会话引用 → `workspace_in_use`
    /// （**不级联、不改派**；引用检查在存储事务内复核）；**只删行、不删磁盘文件**；
    /// 无引用 → 删除并写审计 `workspace.deleted`。
    fn workspace_delete(&self, request: &WorkspaceDeleteRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let workspace_id = WorkspaceId::new(request.workspace_id.clone())
            .map_err(|error| IpcError::internal(format!("workspace_id 非法：{error}")))?;
        self.call(async move {
            let workspace = reads
                .workspace(&workspace_id)
                .await
                .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                .ok_or_else(|| IpcError::workspace_not_found(workspace_id.as_str()))?;
            let now = now_ms();
            let outcome = write
                .execute(StoreCommand::DeleteWorkspace {
                    id: workspace_id.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("工作区删除失败：{error}")))?;
            match outcome {
                StoreOutcome::WorkspaceInUse { sessions } => {
                    return Err(IpcError::workspace_in_use(workspace_id.as_str(), sessions));
                }
                StoreOutcome::Applied { affected } => {
                    if affected == 0 {
                        return Err(IpcError::workspace_not_found(workspace_id.as_str()));
                    }
                }
                other => {
                    return Err(IpcError::internal(format!(
                        "工作区删除返回了非预期结果：{other:?}"
                    )));
                }
            }
            insert_workspace_audit(
                &write,
                "workspace.deleted",
                json!({
                    "workspace_id": workspace_id.as_str(),
                    "name": workspace.name,
                    "root_path": workspace.root_path,
                    "sessions": 0,
                    "disk_untouched": true,
                }),
                now,
            )
            .await?;
            Ok(json!({
                "workspace_id": workspace_id.as_str(),
                "deleted": true,
                // ADR-026 §2.1-1：只删行，不删磁盘文件（回归断言锚点）。
                "disk_untouched": true,
            }))
        })
    }

    /// M7-08 `session_workspace_assign`（ADR-026 §2.1-1/2）：会话改派工作区。
    ///
    /// - 在途 run（`running`/`queued`）或 `waiting_permission`（含 `creating` 过渡态）
    ///   → `invalid_value` + `session_busy` 稳定业务码前缀（ADR-018 附录 A-7）；
    /// - 改派后**记忆注入源即时切换**（按目标工作区根重合成 `system_prompt` 的记忆块；
    ///   既有预设块原样保留——预设内容不随改派重算）；
    /// - **权限基准不重算**（R5/R15，不变量 11）：创建时快照优先；未登记（升级前会话）
    ///   按当前 `workspace_id` 派生后固定（防改派放大权限）；
    /// - 写审计 `session.workspace_assigned`（detail 显式记录「权限基准不重算」）。
    fn session_workspace_assign(
        &self,
        request: &SessionWorkspaceAssignRequest,
    ) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let permissions = self.permissions.clone();
        let session_id = parse_session_id(&request.session_id)?;
        let workspace_id = WorkspaceId::new(request.workspace_id.clone())
            .map_err(|error| IpcError::internal(format!("workspace_id 非法：{error}")))?;
        self.call(async move {
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .ok_or_else(|| assign_session_not_found(&session_id))?;
            // 在途前置：状态非空闲或存在 queued/running run → session_busy（不转移）。
            let active_run = reads
                .session_has_active_run(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("在途 run 读取失败：{error}")))?;
            if is_session_busy(session.status, active_run) {
                return Err(IpcError::invalid_value(format!(
                    "session_busy：会话 {} 在途（status={}，active_run={active_run}），不可改派工作区；\
                     请先等待 run 结束或中断",
                    session_id.as_str(),
                    session.status.as_str()
                )));
            }
            let target = reads
                .workspace(&workspace_id)
                .await
                .map_err(|error| IpcError::internal(format!("工作区读取失败：{error}")))?
                .ok_or_else(|| IpcError::workspace_not_found(workspace_id.as_str()))?;
            let previous = session.workspace_id.clone();
            let baseline = match session_baseline_from_config(&session.config) {
                Some(root) => root,
                None => match &session.workspace_id {
                    Some(current) => reads
                        .workspace(current)
                        .await
                        .map_err(|error| {
                            IpcError::internal(format!("工作区读取失败：{error}"))
                        })?
                        .map(|workspace| std::path::PathBuf::from(workspace.root_path)),
                    None => None,
                },
            };
            if let Some(service) = &permissions {
                service.set_session_baseline(&session_id, baseline.as_deref());
            }
            if previous.as_ref() == Some(&workspace_id) {
                // 幂等：不重复改写 `workspace_id`/提示词；升级前会话首次改派时仍固化基准
                // 快照（未登记 → 按当前工作区派生），使跨重启语义稳定。
                if session_baseline_from_config(&session.config).is_none() {
                    let config =
                        config_with_session_baseline(&session.config, baseline.as_deref());
                    if let Err(error) = write
                        .execute(StoreCommand::UpdateSessionConfig {
                            session_id: session_id.clone(),
                            config,
                            updated_at: now_ms(),
                        })
                        .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %error,
                            "幂等改派的基准快照落库失败（进程内基准仍生效）"
                        );
                    }
                }
                return Ok(assign_response(
                    &session_id,
                    &workspace_id,
                    previous.as_ref(),
                    baseline.as_deref(),
                    false,
                ));
            }
            let config = config_with_session_baseline(&session.config, baseline.as_deref());
            let memory = compose_memory_injection(Path::new(&target.root_path)).render();
            let system_prompt =
                replace_workspace_memory_block(session.system_prompt.as_deref(), memory);
            let now = now_ms();
            let outcome = write
                .execute(StoreCommand::AssignSessionWorkspace {
                    session_id: session_id.clone(),
                    workspace_id: workspace_id.clone(),
                    config,
                    system_prompt: system_prompt.clone(),
                    updated_at: now,
                })
                .await
                .map_err(|error| IpcError::internal(format!("会话改派落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(assign_session_not_found(&session_id));
                }
            }
            write
                .execute(StoreCommand::InsertAudit {
                    record: AuditLogRecord {
                        id: new_audit_id(),
                        session_id: Some(session_id.as_str().to_owned()),
                        runtime_id: Some(session.runtime_id.as_str().to_owned()),
                        actor: "user".to_owned(),
                        action: "session.workspace_assigned".to_owned(),
                        resource: Some("session".to_owned()),
                        detail: Some(
                            json!({
                                "from": previous.as_ref().map(WorkspaceId::as_str),
                                "to": workspace_id.as_str(),
                                "root_path": target.root_path,
                                "baseline_root": baseline.as_ref().map(|path| path.to_string_lossy().to_string()),
                                "baseline_recomputed": false,
                                "memory_injection_switched": true,
                            })
                            .to_string(),
                        ),
                        result: Some("updated".to_owned()),
                        ts: now,
                    },
                })
                .await
                .map_err(|error| IpcError::internal(format!("审计落库失败：{error}")))?;
            Ok(assign_response(
                &session_id,
                &workspace_id,
                previous.as_ref(),
                baseline.as_deref(),
                true,
            ))
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

    // ===== M7-09（ADR-026 §2.2）：Agent 预设四命令（薄转发 [`PresetControl`]；
    // 校验/内置保护/审计语义全部在 preset_control，本层不复刻）。=====

    fn presets_list(&self, request: &PresetsListRequest) -> Result<Value, IpcError> {
        self.presets_required()?.presets_list(request)
    }

    fn preset_add(&self, request: &PresetAddRequest) -> Result<Value, IpcError> {
        self.presets_required()?.preset_add(request)
    }

    fn preset_update(&self, request: &PresetUpdateRequest) -> Result<Value, IpcError> {
        self.presets_required()?.preset_update(request)
    }

    fn preset_delete(&self, request: &PresetDeleteRequest) -> Result<Value, IpcError> {
        self.presets_required()?.preset_delete(request)
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

    // M7-06/ADR-024：连通性测试与模型获取（薄转发；网络/错误码/upsert 语义在
    // provider_control，本层不复刻）。
    fn provider_test(&self, request: &ProviderTestRequest) -> Result<Value, IpcError> {
        self.providers_required()?.provider_test(request)
    }

    fn provider_models_fetch(
        &self,
        request: &ProviderModelsFetchRequest,
    ) -> Result<Value, IpcError> {
        self.providers_required()?.provider_models_fetch(request)
    }

    fn session_send(&self, request: &SessionSendRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let reads = self.reads.clone();
        let supervisor = self.supervisor.clone();
        let executor = self.executor.clone();
        let write = self.write.clone();
        let provider_env = self.provider_env.clone();
        let session_id = request.session_id.clone();
        let text = request.text.clone();
        let client_msg_id = request.client_msg_id.clone();
        let request_provider = request.provider_id.clone();
        let request_model = request.model.clone();
        let thinking_override = request.thinking_depth_value()?;
        // M7-06/ADR-024：引用在命令层已校验（canonicalize + 可访问性）；此处解析为
        // 内核形态（canonical 路径 + kind）随 `session.send` 透传。
        let references = request.resolved_references()?;
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let session_row = match &reads {
                Some(reads) => reads
                    .session(&session_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?,
                None => None,
            };
            // ADR-028 v0.9：三官方运行时旧会话补选（21 行真相表；未列组合默认拒绝）。
            if let (Some(reads), Some(session)) = (&reads, &session_row) {
                if aether_core::supports_multi_provider(session.runtime_id.as_str()) {
                    let providers = reads
                        .providers()
                        .await
                        .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
                    let models = reads.provider_models().await.map_err(|error| {
                        IpcError::internal(format!("供应商模型读取失败：{error}"))
                    })?;
                    let plan = plan_session_selection(
                        session,
                        &providers,
                        &models,
                        request_provider.as_deref(),
                        request_model.as_deref(),
                    )?;
                    if let Some(plan) = plan {
                        // 补选仅空闲/无在途 run；有在途 run → `session_busy`（不排队）。
                        // 第 21 行已在 plan 内先返回 `invalid_value`（优先级见 ADR）。
                        if reads
                            .session_has_active_run(&session_id)
                            .await
                            .map_err(|error| {
                                IpcError::internal(format!("在途 run 检查失败：{error}"))
                            })?
                        {
                            return Err(selection_busy_error());
                        }
                        let (provider_id, new_model) = match &plan {
                            SelectionPlan::Provider { provider_id, model } => {
                                (provider_id.clone(), model.clone())
                            }
                            SelectionPlan::Model { model } => (
                                session_provider_id(session)
                                    .map(str::to_owned)
                                    .ok_or_else(|| IpcError::invalid_value("请选择供应商"))?,
                                Some(model.clone()),
                            ),
                        };
                        // 校验目标（provider 启用 + model 归属/启用）——失败不准备、
                        // 不持久化、保持只读。
                        let target_model = new_model.as_deref().or(session.model.as_deref());
                        if let Some(model) = target_model {
                            validate_selection_target(&provider_id, model, &providers, &models)?;
                        } else {
                            return Err(IpcError::invalid_value("请选择模型"));
                        }
                        // 创建前同等预检（broker + key 可读 + 端点）；失败不准备、不持久化。
                        let protocol =
                            crate::provider_env::runtime_protocol(session.runtime_id.as_str())
                                .ok_or_else(|| {
                                    IpcError::invalid_value("运行时没有可用的供应商协议")
                                })?;
                        let Some(provider_env) = provider_env.as_ref() else {
                            return Err(IpcError::core_not_ready(
                                "凭据 broker 未接线：旧会话补选失败（不回退单供应商 env）",
                            ));
                        };
                        provider_env
                            .preflight_credential(&provider_id, protocol)
                            .await
                            .map_err(|error| {
                                if error.contains("broker") || error.contains("未接线") {
                                    IpcError::core_not_ready(error)
                                } else {
                                    IpcError::invalid_value(error)
                                }
                            })?;
                        // 准备成功判定 = 配置物化 + 原生状态采用 + 适配器 ack；
                        // 失败 → `core_not_ready`，不持久化、不重建原生会话。
                        let executor = executor.as_ref().ok_or_else(|| {
                            IpcError::core_not_ready("执行器未接线：无法准备旧会话补选（不持久化）")
                        })?;
                        executor
                            .prepare_session_selection(
                                &session_id,
                                &provider_id,
                                new_model.as_deref(),
                            )
                            .await
                            .map_err(|error| {
                                IpcError::core_not_ready(format!(
                                    "适配器准备失败（未持久化，保持只读）：{}",
                                    error.message
                                ))
                            })?;
                        let write = write.as_ref().ok_or_else(|| {
                            IpcError::core_not_ready(
                                "存储写路径未接线：补选成功后无法持久化（不发送）",
                            )
                        })?;
                        let mut config = session.config.clone();
                        let Some(object) = config.as_object_mut() else {
                            return Err(IpcError::internal(
                                "会话 config 不是 JSON 对象（不变量破坏）",
                            ));
                        };
                        let aether = object
                            .entry("aether".to_owned())
                            .or_insert_with(|| json!({}));
                        let Some(aether) = aether.as_object_mut() else {
                            return Err(IpcError::internal(
                                "会话 config.aether 不是 JSON 对象（不变量破坏）",
                            ));
                        };
                        aether.insert("provider_id".to_owned(), Value::String(provider_id.clone()));
                        let persisted_model = new_model.or_else(|| session.model.clone());
                        let outcome = write
                            .execute(StoreCommand::UpdateSessionSelection {
                                session_id: session_id.clone(),
                                config,
                                model: persisted_model,
                                updated_at: now_ms(),
                            })
                            .await
                            .map_err(|error| {
                                IpcError::internal(format!("补选持久化失败：{error}"))
                            })?;
                        if let StoreOutcome::Applied { affected } = outcome {
                            if affected == 0 {
                                return Err(IpcError::internal(
                                    "补选持久化未影响任何行（不变量破坏）",
                                ));
                            }
                        }
                    }
                }
            }
            // M3-10/ADR-010 能力门（同步判定路径，尽力而为）：run 执行前的 runtime
            // ready 快照；未 ready（cold/starting）→ 延迟判定（不产生响应警告，
            // run 启动时由执行器按同一门再判定）。覆盖请求仅影响本次 run。
            let warnings = match (&reads, &supervisor) {
                (Some(_reads), Some(supervisor)) => {
                    let mut warnings = None;
                    if let Some(session) = session_row {
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
                .send_with_references(
                    &session_id,
                    &text,
                    &client_msg_id,
                    thinking_override,
                    references,
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
            // M7-02/ADR-022 附录 A-5：超闸门进入全局 FIFO 时附 `queue: { position, limit }`。
            if let Some(queue) = ack.queue {
                response["queue"] = json!({
                    "position": queue.position,
                    "limit": queue.limit,
                });
            }
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

    /// M7-03/ADR-017 `session_pause`：能力门（`pause_resume`）实时复核 → 核心层
    /// 受控中断收口 → `paused`。未声明/连接未就绪 → `pause_unsupported`（不转移、
    /// 不产生假暂停）；非 `idle`/`running`/`waiting_permission` → `invalid_value`。
    fn session_pause(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let reads = self.reads_required()?.clone();
        let supervisor = self.supervisor.clone().ok_or_else(|| {
            IpcError::core_not_ready("监督器未接线：无法复核 pause_resume 能力（启动序列未完成）")
        })?;
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .ok_or_else(|| IpcError::session_not_found(session_id.as_str()))?;
            ensure_pause_supported(&supervisor, &session.runtime_id).await?;
            let report = manager
                .pause(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": report.session_id.as_str(),
                "status": report.to.as_str(),
                "from": report.from.as_str(),
                "interrupted_run": report.interrupted_run.as_ref().map(|run| run.as_str()),
                "waiting_kept": report.waiting_kept.as_ref().map(|run| run.as_str()),
            }))
        })
    }

    /// M7-03/ADR-017 `session_resume`：能力门 + 运行时可用性（disabled/未注册 →
    /// 拒绝并保持 `paused`）→ 有标记重放（`paused → running`）/ 无标记 `paused → idle`。
    fn session_resume(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let reads = self.reads_required()?.clone();
        let supervisor = self.supervisor.clone().ok_or_else(|| {
            IpcError::core_not_ready("监督器未接线：无法复核运行时可用性（启动序列未完成）")
        })?;
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .ok_or_else(|| IpcError::session_not_found(session_id.as_str()))?;
            // ADR-028 v0.9：provider 失效只读；model 失效/缺失补选前拒绝恢复。
            ensure_session_writable(&reads, &session).await?;
            ensure_pause_supported(&supervisor, &session.runtime_id).await?;
            ensure_runtime_available(&reads, &supervisor, &session.runtime_id).await?;
            let report = manager
                .resume(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            let mut response = json!({
                "session_id": report.session_id.as_str(),
                "status": report.to.as_str(),
                "from": report.from.as_str(),
            });
            if let Some(run_id) = report.run_id {
                response["run_id"] = json!(run_id.as_str());
            }
            Ok(response)
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
        let reads = self.reads.clone();
        let run_id = request.run_id.clone();
        self.call(async move {
            let run_id = aether_core::RunId::new(run_id)
                .map_err(|error| IpcError::internal(format!("run_id 非法：{error}")))?;
            // ADR-028 v0.9：provider 失效只读 → 拒绝 run_retry；model 空/失效 →
            // 补选前拒绝（不自动重建/切换）。
            if let Some(reads) = &reads {
                let run = reads
                    .run(&run_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("run 读取失败：{error}")))?
                    .ok_or_else(|| IpcError::invalid_value("run 不存在；无法重试"))?;
                let session = reads
                    .session(&run.session_id)
                    .await
                    .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                    .ok_or_else(|| IpcError::session_not_found(run.session_id.as_str()))?;
                ensure_session_writable(reads, &session).await?;
            }
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
            // M7-07/ADR-025 §2.1-4：删除后 `messages_page`/补读 → `session_not_found`
            // （events 不级联，但会话行已不存在；不得把孤立事件当作有效补读页）。
            if reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .is_none()
            {
                return Err(IpcError::session_not_found(session_id.as_str()));
            }
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

    /// M3-03 `permission_resolve`：用户决议（D9；M6-01/ADR-019 启用 `always` 持久授权）。
    ///
    /// DTO 决策 → 控制层（`once` → allow+once；`session` → allow+session；
    /// `always` → allow+always（「记住」；绑定 canonical target，重启重建））；
    /// `deny` → deny。
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
            crate::ipc::dto::PermissionDecision::Always => {
                (PermissionDecision::Allow, Some(PermissionScope::Always))
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

    /// M6-01 DoD8：`session_permission_mode_set`——会话级权限模式设置。
    ///
    /// 语义（ADR-014 决策 3）：档位为判定权威入参，仅影响后续工具调用决策；
    /// 在途会话沙箱参数不变（投影由 M7-06 承接）；档位变更入审计
    /// （`session.permission_mode_changed`，actor=user，resource=session）。
    fn session_permission_mode_set(
        &self,
        request: &SessionPermissionModeSetRequest,
    ) -> Result<Value, IpcError> {
        let session_id = parse_session_id(&request.session_id)?;
        let mode = request.permission_mode_value()?;
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        self.call(async move {
            let session = reads
                .session(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("会话读取失败：{error}")))?
                .ok_or_else(|| session_not_found(&session_id))?;
            let previous = session.permission_mode;
            let now = now_ms();
            let outcome = write
                .execute(StoreCommand::UpdateSessionPermissionMode {
                    session_id: session_id.clone(),
                    permission_mode: mode,
                    updated_at: now,
                })
                .await
                .map_err(|error| IpcError::internal(format!("会话档位落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(session_not_found(&session_id));
                }
            }
            write
                .execute(StoreCommand::InsertAudit {
                    record: AuditLogRecord {
                        id: new_audit_id(),
                        session_id: Some(session_id.as_str().to_owned()),
                        runtime_id: None,
                        actor: "user".to_owned(),
                        action: "session.permission_mode_changed".to_owned(),
                        resource: Some("session".to_owned()),
                        detail: Some(
                            json!({
                                "from": previous.as_str(),
                                "to": mode.as_str(),
                            })
                            .to_string(),
                        ),
                        result: Some("updated".to_owned()),
                        ts: now,
                    },
                })
                .await
                .map_err(|error| IpcError::internal(format!("审计落库失败：{error}")))?;
            Ok(json!({
                "session_id": session_id.as_str(),
                "permission_mode": mode.as_str(),
                "previous_permission_mode": previous.as_str(),
            }))
        })
    }

    /// M6-01 DoD4：`rules_list`——用户自定义权限规则清单（按创建时间升序）。
    fn rules_list(&self) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        self.call(async move {
            let rules = reads
                .permission_rules()
                .await
                .map_err(|error| IpcError::internal(format!("规则清单读取失败：{error}")))?;
            Ok(Value::Array(rules.iter().map(rule_record_json).collect()))
        })
    }

    /// M6-01 DoD4：`rule_add`——新增规则（形态校验 + `path` canonicalize；
    /// 非法规则拒绝且不落库；`priority` 缺省 0）。
    fn rule_add(&self, request: &RuleAddRequest) -> Result<Value, IpcError> {
        let write = self.write_required()?.clone();
        let service = self.permissions_required()?.clone();
        let name = request.name.clone();
        let resource_code = request.resource.clone();
        let action = request.action.clone();
        let target_kind_code = request.target_kind.clone();
        let target = request.target.clone();
        let decision = request.decision.clone();
        let priority = request.priority_value()?;
        self.call(async move {
            let resource = PermissionResource::from_code(&resource_code)
                .ok_or_else(|| IpcError::invalid_enum("resource", "资源枚举非法"))?;
            let target_kind = RuleTargetKind::from_code(&target_kind_code)
                .ok_or_else(|| IpcError::invalid_enum("target_kind", "目标类型枚举非法"))?;
            ensure_rule_resource_action(resource, &action)?;
            let normalized = normalize_rule_target(resource, target_kind, target.as_deref())
                .map_err(map_rule_error)?;
            let now = now_ms();
            let record = PermissionRuleRecord {
                id: new_rule_id(),
                name: name.clone(),
                resource: resource_code.clone(),
                action: action.clone(),
                target_kind: target_kind_code.clone(),
                target: normalized,
                decision: decision.clone(),
                priority,
                enabled: true,
                created_at: now,
                updated_at: now,
            };
            let outcome = write
                .execute(StoreCommand::InsertPermissionRule {
                    rule: record.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("规则落库失败：{error}")))?;
            ensure_rule_applied(outcome)?;
            insert_rule_audit(
                &write,
                "rule.created",
                &record.id,
                "created",
                json!({
                    "name": record.name,
                    "resource": record.resource,
                    "action": record.action,
                    "target_kind": record.target_kind,
                    "target": record.target,
                    "decision": record.decision,
                    "priority": record.priority,
                }),
            )
            .await?;
            service
                .restore_rules()
                .await
                .map_err(map_permission_error)?;
            Ok(rule_record_json(&record))
        })
    }

    /// M6-01 DoD4：`rule_update`——更新规则（`rule_id` 不存在 → `rule_not_found`；
    /// `resource` 变更须同时提供匹配的 `action`）。
    fn rule_update(&self, request: &RuleUpdateRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let service = self.permissions_required()?.clone();
        let rule_id = request.rule_id.clone();
        let patch = RuleUpdatePatch {
            name: request.name.clone(),
            resource: request.resource.clone(),
            action: request.action.clone(),
            target_kind: request.target_kind.clone(),
            target: request.target.clone(),
            decision: request.decision.clone(),
            priority: request.priority,
        };
        self.call(async move {
            let existing = reads
                .permission_rules()
                .await
                .map_err(|error| IpcError::internal(format!("规则清单读取失败：{error}")))?
                .into_iter()
                .find(|rule| rule.id == rule_id)
                .ok_or_else(|| IpcError::rule_not_found(&rule_id))?;
            if patch.resource.is_some() && patch.action.is_none() {
                return Err(IpcError::invalid_value(
                    "resource 变更须同时提供匹配的 action（ADR-018 附录 A-1）",
                ));
            }
            let resource_code = patch
                .resource
                .clone()
                .unwrap_or_else(|| existing.resource.clone());
            let action = patch
                .action
                .clone()
                .unwrap_or_else(|| existing.action.clone());
            let target_kind_code = patch
                .target_kind
                .clone()
                .unwrap_or_else(|| existing.target_kind.clone());
            let resource = PermissionResource::from_code(&resource_code)
                .ok_or_else(|| IpcError::invalid_enum("resource", "资源枚举非法"))?;
            let target_kind = RuleTargetKind::from_code(&target_kind_code)
                .ok_or_else(|| IpcError::invalid_enum("target_kind", "目标类型枚举非法"))?;
            ensure_rule_resource_action(resource, &action)?;
            let target = match target_kind {
                RuleTargetKind::Any => None,
                _ => patch.target.clone().or_else(|| existing.target.clone()),
            };
            let normalized = normalize_rule_target(resource, target_kind, target.as_deref())
                .map_err(map_rule_error)?;
            let record = PermissionRuleRecord {
                id: existing.id.clone(),
                name: patch.name.clone().unwrap_or_else(|| existing.name.clone()),
                resource: resource_code,
                action,
                target_kind: target_kind_code,
                target: normalized,
                decision: patch
                    .decision
                    .clone()
                    .unwrap_or_else(|| existing.decision.clone()),
                priority: patch.priority.unwrap_or(existing.priority),
                enabled: existing.enabled,
                created_at: existing.created_at,
                updated_at: now_ms(),
            };
            let outcome = write
                .execute(StoreCommand::UpdatePermissionRule {
                    rule: record.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("规则落库失败：{error}")))?;
            ensure_rule_applied(outcome)?;
            insert_rule_audit(
                &write,
                "rule.updated",
                &record.id,
                "updated",
                json!({
                    "before": rule_record_json(&existing),
                    "after": rule_record_json(&record),
                }),
            )
            .await?;
            service
                .restore_rules()
                .await
                .map_err(map_permission_error)?;
            Ok(rule_record_json(&record))
        })
    }

    /// M6-01 DoD4：`rule_delete`——删除规则（`rule_id` 不存在 → `rule_not_found`）。
    fn rule_delete(&self, request: &RuleDeleteRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let service = self.permissions_required()?.clone();
        let rule_id = request.rule_id.clone();
        self.call(async move {
            let existing = reads
                .permission_rules()
                .await
                .map_err(|error| IpcError::internal(format!("规则清单读取失败：{error}")))?
                .into_iter()
                .find(|rule| rule.id == rule_id)
                .ok_or_else(|| IpcError::rule_not_found(&rule_id))?;
            let outcome = write
                .execute(StoreCommand::DeletePermissionRule {
                    id: rule_id.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("规则删除失败：{error}")))?;
            ensure_rule_applied(outcome)?;
            insert_rule_audit(
                &write,
                "rule.deleted",
                &rule_id,
                "deleted",
                json!({ "name": existing.name }),
            )
            .await?;
            service
                .restore_rules()
                .await
                .map_err(map_permission_error)?;
            Ok(json!({ "rule_id": rule_id, "deleted": true }))
        })
    }

    /// M6-01 DoD4：`rule_toggle`——启用/停用规则（`rule_id` 不存在 → `rule_not_found`）。
    fn rule_toggle(&self, request: &RuleToggleRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let write = self.write_required()?.clone();
        let service = self.permissions_required()?.clone();
        let rule_id = request.rule_id.clone();
        let enabled = request.enabled;
        self.call(async move {
            let existing = reads
                .permission_rules()
                .await
                .map_err(|error| IpcError::internal(format!("规则清单读取失败：{error}")))?
                .into_iter()
                .find(|rule| rule.id == rule_id)
                .ok_or_else(|| IpcError::rule_not_found(&rule_id))?;
            let now = now_ms();
            let outcome = write
                .execute(StoreCommand::SetPermissionRuleEnabled {
                    id: rule_id.clone(),
                    enabled,
                    updated_at: now,
                })
                .await
                .map_err(|error| IpcError::internal(format!("规则落库失败：{error}")))?;
            ensure_rule_applied(outcome)?;
            insert_rule_audit(
                &write,
                "rule.toggled",
                &rule_id,
                "toggled",
                json!({ "enabled": enabled, "name": existing.name }),
            )
            .await?;
            service
                .restore_rules()
                .await
                .map_err(map_permission_error)?;
            Ok(json!({ "rule_id": rule_id, "enabled": enabled }))
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

    fn runtime_disable(
        &self,
        request: &crate::ipc::dto::RuntimeDisableRequest,
    ) -> Result<Value, IpcError> {
        self.inner.runtime_disable(request)
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

    fn session_export(
        &self,
        request: &crate::ipc::dto::SessionExportRequest,
        canonical_target_dir: Option<&std::path::Path>,
    ) -> Result<Value, IpcError> {
        self.inner.session_export(request, canonical_target_dir)
    }

    fn archive_export(
        &self,
        request: &crate::ipc::dto::ArchiveExportRequest,
        canonical_target_dir: Option<&std::path::Path>,
    ) -> Result<Value, IpcError> {
        self.inner.archive_export(request, canonical_target_dir)
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

/// `rule_update` 补丁字段（`None` = 不变；字段级校验在 DTO 层先行完成）。
#[derive(Debug, Clone, Default)]
struct RuleUpdatePatch {
    name: Option<String>,
    resource: Option<String>,
    action: Option<String>,
    target_kind: Option<String>,
    target: Option<String>,
    decision: Option<String>,
    priority: Option<i64>,
}

/// 新 ULID（规则 id / 审计 id；与核心 `ulid` 生成器同源，26 位 Crockford Base32）。
fn new_rule_id() -> String {
    ulid::Ulid::new().to_string()
}

fn new_audit_id() -> String {
    ulid::Ulid::new().to_string()
}

/// 规则 `resource × action` 匹配校验（ADR-019 附录 A-1：`fs.read→read` 等）。
fn ensure_rule_resource_action(resource: PermissionResource, action: &str) -> Result<(), IpcError> {
    if resource.rule_action() != action {
        return Err(IpcError::invalid_value(format!(
            "resource={} 要求 action={}，收到 {action}（ADR-019 兼容矩阵）",
            resource.as_str(),
            resource.rule_action()
        )));
    }
    Ok(())
}

/// 规则写命令必须影响 1 行（0 行 = 不变量破坏）。
fn ensure_rule_applied(outcome: StoreOutcome) -> Result<(), IpcError> {
    if let StoreOutcome::Applied { affected } = outcome {
        if affected == 0 {
            return Err(IpcError::internal("规则写入未影响任何行（不变量破坏）"));
        }
    }
    Ok(())
}

/// 规则条目响应形状（`rules_list` / `rule_*` 回执；字段与 `permission_rules` 表一致）。
fn rule_record_json(record: &PermissionRuleRecord) -> Value {
    json!({
        "id": record.id,
        "name": record.name,
        "resource": record.resource,
        "action": record.action,
        "target_kind": record.target_kind,
        "target": record.target,
        "decision": record.decision,
        "priority": record.priority,
        "enabled": record.enabled,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

/// 规则变更审计（M6-01/ADR-019 附录 A-3：`rule.created/updated/deleted/toggled`）。
async fn insert_rule_audit(
    write: &WriteQueue,
    action: &str,
    rule_id: &str,
    result: &str,
    detail: Value,
) -> Result<(), IpcError> {
    write
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: new_audit_id(),
                session_id: None,
                runtime_id: None,
                actor: "user".to_owned(),
                action: action.to_owned(),
                resource: Some(format!("rule:{rule_id}")),
                detail: Some(detail.to_string()),
                result: Some(result.to_owned()),
                ts: now_ms(),
            },
        })
        .await
        .map_err(|error| IpcError::internal(format!("审计落库失败：{error}")))?;
    Ok(())
}

/// 规则校验错误 → 结构化 IPC 错误（ADR-019 附录 A-1：非法组合 → `invalid_enum`）。
fn map_rule_error(error: RuleError) -> IpcError {
    match error {
        RuleError::TargetKindMismatch { .. } => {
            IpcError::invalid_enum("target_kind", error.message())
        }
        RuleError::ResourceActionMismatch { .. } | RuleError::InvalidTargetForm { .. } => {
            IpcError::invalid_value(error.message())
        }
    }
}

/// 会话不存在（引用命令面：`artifacts_list` / `artifact_add`）。
fn session_not_found(session_id: &SessionId) -> IpcError {
    IpcError::invalid_value(format!(
        "session_id 不存在（{}）；引用操作仅对既有会话可用",
        session_id.as_str()
    ))
}

/// 会话不存在（M7-08 `session_workspace_assign`；`session_not_found` 稳定业务码前缀，
/// 口径同 ADR-018 附录 A-7：不入错误码枚举、UI 按 message 展示）。
fn assign_session_not_found(session_id: &SessionId) -> IpcError {
    IpcError::invalid_value(format!(
        "session_not_found：会话不存在（{}）；无法改派工作区",
        session_id.as_str()
    ))
}

// ===== M7-08：工作区/权限基准辅助 =====

/// 会话当前是否「在途」（ADR-026 §2.1-1 改派前置）：非空闲会话状态或存在在途 run。
///
/// 口径：`creating` / `running` / `waiting_permission` 或 `queued`/`running` run →
/// `session_busy`；`paused` 无在途 run（受控中断已收口）不阻断——与 ADR 文本
/// 「在途 run（`running`/`queued`）或 `waiting_permission`」一致。
fn is_session_busy(status: SessionStatus, active_run: bool) -> bool {
    active_run
        || matches!(
            status,
            SessionStatus::Creating | SessionStatus::Running | SessionStatus::WaitingPermission
        )
}

/// 工作区响应条目（`workspaces_list` / `workspace_create` / `workspace_update`）。
fn workspace_json(workspace: &Workspace, count: aether_store::WorkspaceSessionCount) -> Value {
    json!({
        "workspace_id": workspace.id.as_str(),
        "name": workspace.name,
        "root_path": workspace.root_path,
        // ADR-026 §2.1-4：P1 不启用（恒空数组；字段保留待 P4 记忆检索时重议）。
        "memory_files": workspace.memory_files,
        "active_sessions": count.active,
        "total_sessions": count.total,
        "created_at": workspace.created_at,
        "updated_at": workspace.updated_at,
    })
}

/// 工作区管理审计（`workspace.created` / `workspace.updated` / `workspace.deleted`；
/// actor=`user`，ADR-026 附录 A-3）。
async fn insert_workspace_audit(
    write: &WriteQueue,
    action: &str,
    detail: Value,
    ts: i64,
) -> Result<(), IpcError> {
    write
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: new_audit_id(),
                session_id: None,
                runtime_id: None,
                actor: "user".to_owned(),
                action: action.to_owned(),
                resource: Some("workspace".to_owned()),
                detail: Some(detail.to_string()),
                result: Some("updated".to_owned()),
                ts,
            },
        })
        .await
        .map_err(|error| IpcError::internal(format!("工作区审计落库失败：{error}")))?;
    Ok(())
}

/// 改派响应（ADR-026 §2.1-1；UI 提示「权限基准不变，如需变更请新建会话」）。
fn assign_response(
    session_id: &SessionId,
    workspace_id: &WorkspaceId,
    previous: Option<&WorkspaceId>,
    baseline_root: Option<&Path>,
    changed: bool,
) -> Value {
    json!({
        "session_id": session_id.as_str(),
        "workspace_id": workspace_id.as_str(),
        "previous_workspace_id": previous.map(WorkspaceId::as_str),
        "changed": changed,
        // R15：改派不重算权限基准（防改派放大权限）。
        "baseline_unchanged": true,
        "baseline_root": baseline_root.map(|path| path.to_string_lossy().to_string()),
        "memory_injection_switched": changed,
        "notice": "权限基准不变，如需变更请新建会话",
    })
}

/// 会话创建时固定权限基准（M7-08/R15）：进程内登记 + 持久化快照。
///
/// 持久化失败不阻断会话创建（进程内基准已生效；重启后该会话按「未登记」沿用缺省
/// 基准——保守方向：不放大权限），仅告警。
async fn persist_session_baseline(
    write: Option<&WriteQueue>,
    permissions: Option<&PermissionService>,
    session: &aether_core::Session,
    baseline: Option<&Path>,
) {
    if let Some(service) = permissions {
        service.set_session_baseline(&session.id, baseline);
    }
    let Some(write) = write else {
        return;
    };
    let config = config_with_session_baseline(&session.config, baseline);
    if config == session.config {
        return;
    }
    if let Err(error) = write
        .execute(StoreCommand::UpdateSessionConfig {
            session_id: session.id.clone(),
            config,
            updated_at: now_ms(),
        })
        .await
    {
        tracing::warn!(
            session_id = %session.id,
            error = %error,
            "权限基准快照落库失败（进程内基准仍生效；重启后按未登记兼容）"
        );
    }
}

/// 替换 `system_prompt` 中的工作区记忆块（记忆注入源切换；D14/ADR-026 §2.1-1）。
///
/// 结构：`[预设块]\n\n[工作区约定记忆块]`（M7-09/ADR-026 合成顺序）。
/// 改派只替换记忆块，**既有预设块原样保留**（预设内容不随改派重算，禁止静默覆盖）；
/// 记忆块按 [`MEMORY_INJECTION_HEADER`] 定位（`rfind`：记忆块恒在末尾），
/// `memory=None` 时仅保留预设块。
fn replace_workspace_memory_block(
    existing: Option<&str>,
    memory: Option<String>,
) -> Option<String> {
    let marker = format!("{MEMORY_INJECTION_HEADER}\n来源：");
    let preset_block = existing
        .map(|text| match text.rfind(&marker) {
            Some(index) => text[..index].trim_end(),
            None => text.trim_end(),
        })
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    match (preset_block, memory) {
        (Some(preset), Some(memory)) => Some(format!("{preset}\n\n{memory}")),
        (Some(preset), None) => Some(preset),
        (None, Some(memory)) => Some(memory),
        (None, None) => None,
    }
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

/// `pause_resume` 能力门（M7-03/ADR-017 附录 A-5：Ready/Degraded 可判；
/// Connecting/Disconnected/未注册/无连接视为未声明 → `pause_unsupported`，
/// 不做任何状态转移）。命令层实时复核（pause 与 resume 共用）。
async fn ensure_pause_supported(
    supervisor: &Supervisor,
    runtime_id: &RuntimeId,
) -> Result<(), IpcError> {
    let Some(runtime) = supervisor.get(runtime_id.as_str()) else {
        return Err(IpcError::pause_unsupported(
            runtime_id.as_str(),
            "运行时未注册",
        ));
    };
    let (reason, supported) = match runtime.connection().await {
        Some(connection) => match connection.state() {
            ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => (
                "未声明 pause_resume 能力",
                hello
                    .runtime
                    .capabilities
                    .iter()
                    .any(|capability| capability == PAUSE_RESUME_CAPABILITY),
            ),
            ConnectionState::Connecting => ("连接未就绪（Connecting）", false),
            ConnectionState::Disconnected(_) => ("连接未就绪（Disconnected）", false),
        },
        None => ("连接未就绪（无活动连接）", false),
    };
    if supported {
        Ok(())
    } else {
        Err(IpcError::pause_unsupported(runtime_id.as_str(), reason))
    }
}

/// ADR-028 v0.9：校验会话级供应商（三官方运行时 `provider_id` + `model` 语义必填）。
///
/// 校验：provider 存在/启用、模型必填且属于该 provider 并启用、运行时协议端点可解析、
/// provider → protocol 兼容、broker 可用且 `api_key_ref` 可读（预检读取后立即丢弃）。
/// 任一失败 → 创建前拒绝，无部分会话；不自动选择默认 provider。
async fn resolve_session_provider(
    reads: Option<&ReadPool>,
    runtime_id: &str,
    requested: Option<&str>,
    model: Option<&str>,
    supports_multi_provider: bool,
    broker: Option<&crate::provider_env::ProviderEnvHook>,
) -> Result<Option<String>, IpcError> {
    if !supports_multi_provider {
        return if requested.is_some() {
            Err(IpcError::invalid_value(format!(
                "运行时 {runtime_id} 暂不支持会话级多供应商（缺少 multi_provider 能力）"
            )))
        } else {
            Ok(None)
        };
    }
    let Some(provider_id) = requested.filter(|value| !value.is_empty()) else {
        return Err(IpcError::invalid_value("请选择供应商"));
    };
    let Some(model) = model.filter(|value| !value.is_empty()) else {
        return Err(IpcError::invalid_value("请选择模型"));
    };
    let Some(reads) = reads else {
        return Err(IpcError::core_not_ready(
            "存储未就绪：无法校验 provider_id/model",
        ));
    };
    let provider = reads
        .provider(provider_id)
        .await
        .map_err(|error| IpcError::internal(format!("读取供应商失败：{error}")))?
        .ok_or_else(|| IpcError::invalid_value(format!("供应商不存在：{provider_id}")))?;
    if !provider.enabled {
        return Err(IpcError::invalid_value(format!(
            "供应商已停用：{provider_id}"
        )));
    }
    let models = reads
        .provider_models()
        .await
        .map_err(|error| IpcError::internal(format!("读取供应商模型失败：{error}")))?;
    let owned = models
        .iter()
        .any(|entry| entry.provider_id == provider_id && entry.model_id == model && entry.enabled);
    if !owned {
        return Err(IpcError::invalid_value(format!(
            "模型 {model} 不属于供应商 {provider_id} 或未启用"
        )));
    }
    let protocol = crate::provider_env::runtime_protocol(runtime_id).ok_or_else(|| {
        IpcError::invalid_value(format!("运行时 {runtime_id} 没有可用的供应商协议"))
    })?;
    crate::provider_env::resolve_protocol_base_url(&provider, protocol).map_err(|error| {
        IpcError::invalid_value(format!(
            "供应商 {provider_id} 未配置运行时 {runtime_id} 所需协议端点：{error}"
        ))
    })?;
    let Some(broker) = broker else {
        return Err(IpcError::core_not_ready(
            "凭据 broker 未接线：multi_provider 会话创建失败（不回退单供应商 env）",
        ));
    };
    broker
        .preflight_credential(provider_id, protocol)
        .await
        .map_err(|error| {
            if error.contains("broker") || error.contains("未接线") {
                IpcError::core_not_ready(error)
            } else {
                IpcError::invalid_value(error)
            }
        })?;
    Ok(Some(provider_id.to_owned()))
}

/// 旧会话补选计划（ADR-028 v0.9 21 行真相表）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionPlan {
    /// 补选 provider（`model` = 一次性覆盖；`None` = 沿用会话现有有效 model）。
    Provider {
        provider_id: String,
        model: Option<String>,
    },
    /// 仅一次性补选 model（provider 不变）。
    Model { model: String },
}

/// 会话当前选择的 provider id（`sessions.config.aether.provider_id`）。
fn session_provider_id(session: &aether_core::Session) -> Option<&str> {
    session
        .config
        .get("aether")
        .and_then(|value| value.get("provider_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 补选并发错误文案（ADR-028 v0.9：不复用等待队列满文案；业务码保留 `session_busy`）。
pub const SESSION_SELECTION_BUSY_MESSAGE: &str =
    "当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。（session_busy）";

fn selection_busy_error() -> IpcError {
    IpcError::invalid_value(SESSION_SELECTION_BUSY_MESSAGE)
}

fn readonly_selection_error() -> IpcError {
    IpcError::invalid_value(
        "供应商已停用/删除：当前会话只读（可查看历史、可中断在途任务），禁止发送/重试/恢复及补选",
    )
}

/// 21 行真相表（ADR-028 v0.9 §2.1-4；未列组合默认拒绝）。
///
/// 返回 `Ok(None)` = 正常发送；`Ok(Some(plan))` = 补选后发送；`Err` = 拒绝。
fn plan_session_selection(
    session: &aether_core::Session,
    providers: &[aether_store::ProviderRecord],
    models: &[aether_store::ProviderModelRecord],
    request_provider: Option<&str>,
    request_model: Option<&str>,
) -> Result<Option<SelectionPlan>, IpcError> {
    let request_provider = request_provider.filter(|value| !value.is_empty());
    let request_model = request_model.filter(|value| !value.is_empty());
    let session_model = session.model.as_deref().filter(|value| !value.is_empty());
    let session_provider = session_provider_id(session);
    let model_active_for = |provider_id: &str, model: &str| {
        models.iter().any(|entry| {
            entry.provider_id == provider_id && entry.model_id == model && entry.enabled
        })
    };
    let provider_active = |provider_id: &str| {
        providers
            .iter()
            .any(|provider| provider.id == provider_id && provider.enabled)
    };
    let model_anywhere = |model: &str| {
        providers
            .iter()
            .any(|provider| provider.enabled && model_active_for(&provider.id, model))
    };
    let session_model_state = match session_provider {
        Some(provider_id) if !provider_active(provider_id) => "invalid",
        Some(provider_id) => match session_model {
            None => "missing",
            Some(model) if model_active_for(provider_id, model) => "active",
            Some(_) => "invalid",
        },
        None => match session_model {
            None => "missing",
            Some(model) if model_anywhere(model) => "active",
            Some(_) => "invalid",
        },
    };
    match session_provider {
        Some(provider_id) => {
            // 第 21 行：provider 失效 → 只读；**优先于并发检查**返回 invalid_value。
            if !provider_active(provider_id) {
                return Err(readonly_selection_error());
            }
            match (request_provider, request_model) {
                // 第 13/17/20 行：provider 已选，禁止切换（即使 model 任意）。
                (Some(_), _) => Err(IpcError::invalid_value(
                    "会话已选择供应商，禁止运行期切换供应商（provider 已选且 model 有效）",
                )),
                // 第 14 行：model 有效时禁止运行期修改。
                (None, Some(_)) if session_model_state == "active" => Err(IpcError::invalid_value(
                    "会话模型有效，禁止运行期修改模型（禁止运行期切换）",
                )),
                // 第 15/18 行：model 空/失效/不属于 → 一次性补选。
                (None, Some(model)) => Ok(Some(SelectionPlan::Model {
                    model: model.to_owned(),
                })),
                // 第 16/19 行：需要补选 model 但未提供。
                (None, None) if session_model_state != "active" => {
                    Err(IpcError::invalid_value("请选择模型（模型缺失或已失效）"))
                }
                // 第 12 行：正常发送。
                (None, None) => Ok(None),
            }
        }
        None => {
            // 第 4/8/11 行：无 provider 且未携带 provider → 拒绝（含“旧会话正常 send”）。
            let Some(provider_id) = request_provider else {
                return Err(IpcError::invalid_value("请选择供应商"));
            };
            match (request_model, session_model_state) {
                // 第 1/6/9 行：补选 provider（+ model 覆盖/沿用/修复失效）。
                (Some(model), _) => Ok(Some(SelectionPlan::Provider {
                    provider_id: provider_id.to_owned(),
                    model: Some(model.to_owned()),
                })),
                // 第 2/10 行：补选 provider 但 model 缺失/失效 → 拒绝。
                (None, "active") => Ok(Some(SelectionPlan::Provider {
                    provider_id: provider_id.to_owned(),
                    model: None,
                })),
                (None, _) => Err(IpcError::invalid_value("请选择模型")),
            }
        }
    }
}

/// 补选目标校验（provider 有效、model 属于该 provider 并启用）。
fn validate_selection_target(
    provider_id: &str,
    model: &str,
    providers: &[aether_store::ProviderRecord],
    models: &[aether_store::ProviderModelRecord],
) -> Result<(), IpcError> {
    if !providers
        .iter()
        .any(|provider| provider.id == provider_id && provider.enabled)
    {
        return Err(IpcError::invalid_value(format!(
            "供应商不存在或已停用：{provider_id}"
        )));
    }
    if !models
        .iter()
        .any(|entry| entry.provider_id == provider_id && entry.model_id == model && entry.enabled)
    {
        return Err(IpcError::invalid_value(format!(
            "模型 {model} 不属于供应商 {provider_id} 或未启用"
        )));
    }
    Ok(())
}

/// 旧会话/只读判定：官方运行时 provider 失效 → 拒绝；model 空/失效 → 拒绝补选前操作。
async fn ensure_session_writable(
    reads: &ReadPool,
    session: &aether_core::Session,
) -> Result<(), IpcError> {
    if !aether_core::supports_multi_provider(session.runtime_id.as_str()) {
        return Ok(());
    }
    let providers = reads
        .providers()
        .await
        .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?;
    let models = reads
        .provider_models()
        .await
        .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
    let Some(provider_id) = session_provider_id(session) else {
        return Err(IpcError::invalid_value("请选择供应商"));
    };
    if !providers
        .iter()
        .any(|provider| provider.id == provider_id && provider.enabled)
    {
        return Err(readonly_selection_error());
    }
    let Some(model) = session.model.as_deref().filter(|value| !value.is_empty()) else {
        return Err(IpcError::invalid_value("请选择模型（模型缺失或已失效）"));
    };
    if !models
        .iter()
        .any(|entry| entry.provider_id == provider_id && entry.model_id == model && entry.enabled)
    {
        return Err(IpcError::invalid_value("请选择模型（模型缺失或已失效）"));
    }
    Ok(())
}

/// 恢复前的运行时可用性复核（M7-03/ADR-017 §2.1-4①：`enabled=0` 或
/// `disabled` → 拒绝并保持 `paused`；注册表缺失同样拒绝）。
async fn ensure_runtime_available(
    reads: &ReadPool,
    supervisor: &Supervisor,
    runtime_id: &RuntimeId,
) -> Result<(), IpcError> {
    let Some(runtime) = supervisor.get(runtime_id.as_str()) else {
        return Err(IpcError::invalid_value(format!(
            "运行时 {} 未注册，无法恢复（保持 paused）；请先修复运行时注册",
            runtime_id.as_str()
        )));
    };
    let enabled = reads
        .runtime_preferences()
        .await
        .map_err(|error| IpcError::internal(format!("运行时启停偏好读取失败：{error}")))?
        .get(runtime_id.as_str())
        .copied()
        .unwrap_or(runtime.manifest().enabled);
    if !enabled {
        return Err(IpcError::invalid_value(format!(
            "运行时 {} 已被用户禁用（enabled=0），无法恢复（保持 paused）；请先启用",
            runtime_id.as_str()
        )));
    }
    let (status, reason) = runtime.summary_snapshot();
    if status == RuntimeStatus::Disabled {
        let detail = reason.map(|reason| reason.as_str()).unwrap_or("disabled");
        return Err(IpcError::invalid_value(format!(
            "运行时 {} 已禁用（{detail}），无法恢复（保持 paused）；请先修复并重新启用",
            runtime_id.as_str()
        )));
    }
    Ok(())
}

/// 生命周期错误 → IPC 结构化错误（状态冲突用 `invalid_value`；内部错误用 `internal`）。
///
/// `session_busy` / `storage_backpressure` / `persist_degraded` 的专用 IPC 错误码
/// 未在本任务登记（避免超出 M3-02 范围新增错误码面）；消息保留稳定业务码，
/// 供 UI 展示与后续 M3-06 降级 UX 消费。
/// M7-03/ADR-017：`SessionPaused`（`session_paused`）与 `PauseState` 同样以
/// `invalid_value` + 稳定业务码/当前状态呈现（ADR-018 附录 A-7 业务码口径；
/// `pause_unsupported` 为 ADR-006 附录 B 登记码，由能力门路径专门返回）。
pub fn map_lifecycle_error(error: LifecycleError) -> IpcError {
    match error {
        LifecycleError::Internal { .. } => IpcError::internal(error.to_string()),
        LifecycleError::Storage { .. } | LifecycleError::Pipeline { .. } => {
            IpcError::internal(error.to_string())
        }
        LifecycleError::SessionNotFound { session_id } => {
            IpcError::session_not_found(session_id.as_str())
        }
        LifecycleError::SessionBusy { .. }
        | LifecycleError::SessionClosed { .. }
        | LifecycleError::InvalidTransition { .. }
        | LifecycleError::RunNotFound { .. }
        | LifecycleError::RunNotRetryable { .. }
        | LifecycleError::SessionPaused { .. }
        | LifecycleError::PauseState { .. }
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

    #[test]
    fn provider_id_validates_model_endpoint_and_capability() {
        use std::sync::Arc;

        use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
        use aether_store::{
            ProviderModelRecord, ProviderRecord, StoreCommand, StoreRuntime, WriteQueueConfig,
        };

        #[derive(Default)]
        struct MemorySecrets {
            values: std::sync::Mutex<std::collections::HashMap<String, String>>,
        }
        impl SecretStore for MemorySecrets {
            fn level(&self) -> SecurityLevel {
                SecurityLevel::OsKeychain
            }
            fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
                self.values
                    .lock()
                    .map_err(|_| SecretError::NotFound {
                        reference: reference.clone(),
                    })?
                    .get(&reference.to_uri())
                    .map(|value| SecretValue::new(value.clone()))
                    .ok_or_else(|| SecretError::NotFound {
                        reference: reference.clone(),
                    })
            }
            fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
                self.values
                    .lock()
                    .map_err(|_| SecretError::NotFound {
                        reference: reference.clone(),
                    })?
                    .insert(reference.to_uri(), value.expose().to_owned());
                Ok(())
            }
            fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
                self.values
                    .lock()
                    .map_err(|_| SecretError::NotFound {
                        reference: reference.clone(),
                    })?
                    .remove(&reference.to_uri())
                    .map(|_| ())
                    .ok_or_else(|| SecretError::NotFound {
                        reference: reference.clone(),
                    })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            runtime.handle(),
        )
        .unwrap();
        let provider_id = "01J0000000000000000000M710";
        runtime
            .block_on(storage.queue().execute(StoreCommand::InsertProvider {
                provider: ProviderRecord {
                    id: provider_id.to_owned(),
                    name: "DeepSeek".to_owned(),
                    provider_type: "deepseek".to_owned(),
                    base_url: Some("https://api.deepseek.com".to_owned()),
                    endpoints: r#"{"anthropic":"https://api.deepseek.com/anthropic"}"#.to_owned(),
                    api_key_ref: Some(format!("keychain://aether/provider/{provider_id}")),
                    enabled: true,
                    is_builtin: false,
                    created_at: 1,
                    updated_at: 10,
                },
            }))
            .unwrap();
        runtime
            .block_on(storage.queue().execute(StoreCommand::InsertProviderModel {
                model: ProviderModelRecord {
                    id: "01J0000000000000000000M711".to_owned(),
                    provider_id: provider_id.to_owned(),
                    model_id: "deepseek-flash".to_owned(),
                    display_name: "DeepSeek Flash".to_owned(),
                    enabled: true,
                    created_at: 1,
                },
            }))
            .unwrap();

        let secrets = Arc::new(MemorySecrets::default());
        let reference = KeychainRef::new("provider", provider_id).unwrap();
        secrets
            .set(&reference, &SecretValue::new("test-key".to_owned()))
            .unwrap();
        let hook = crate::provider_env::ProviderEnvHook::new(dir.path().to_path_buf());
        hook.wire(
            storage.reads().clone(),
            Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
        );
        hook.set_broker_info(crate::credential_broker::BrokerInfo {
            endpoint: "test-endpoint".to_owned(),
            token: "test-token".to_owned(),
        });

        // 三官方运行时语义必填：缺省 provider/model → 创建前拒绝。
        let missing_provider = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "claude-code",
                None,
                Some("deepseek-flash"),
                true,
                Some(&hook),
            ))
            .unwrap_err();
        assert_eq!(missing_provider.code, IpcErrorCode::InvalidValue);
        assert!(missing_provider.message.contains("请选择供应商"));
        let missing_model = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "claude-code",
                Some(provider_id),
                None,
                true,
                Some(&hook),
            ))
            .unwrap_err();
        assert_eq!(missing_model.code, IpcErrorCode::InvalidValue);
        assert!(missing_model.message.contains("请选择模型"));
        assert_eq!(
            runtime
                .block_on(resolve_session_provider(
                    Some(storage.reads()),
                    "claude-code",
                    Some(provider_id),
                    Some("deepseek-flash"),
                    true,
                    Some(&hook),
                ))
                .unwrap(),
            Some(provider_id.to_owned())
        );
        // 显式 provider 与模型归属不一致 → 创建前拒绝。
        let wrong_model = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "claude-code",
                Some(provider_id),
                Some("claude-opus"),
                true,
                Some(&hook),
            ))
            .unwrap_err();
        assert_eq!(wrong_model.code, IpcErrorCode::InvalidValue);
        // 显式 provider 缺少运行时协议端点 → 创建前拒绝。
        let wrong_protocol = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "google",
                Some(provider_id),
                Some("deepseek-flash"),
                true,
                Some(&hook),
            ))
            .unwrap_err();
        assert_eq!(wrong_protocol.code, IpcErrorCode::InvalidValue);
        // 未声明 multi_provider 的运行时拒绝显式 provider_id，避免传了被忽略而串 key。
        let unsupported = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "codex",
                Some(provider_id),
                None,
                false,
                Some(&hook),
            ))
            .unwrap_err();
        assert_eq!(unsupported.code, IpcErrorCode::InvalidValue);
        assert!(unsupported.message.contains("multi_provider"));
        // broker 未接线 → core_not_ready（不得回退单供应商 env）。
        let broker_missing = runtime
            .block_on(resolve_session_provider(
                Some(storage.reads()),
                "claude-code",
                Some(provider_id),
                Some("deepseek-flash"),
                true,
                None,
            ))
            .unwrap_err();
        assert_eq!(broker_missing.code, IpcErrorCode::CoreNotReady);
    }

    fn truth_table_fixture() -> (
        aether_core::Session,
        Vec<aether_store::ProviderRecord>,
        Vec<aether_store::ProviderModelRecord>,
    ) {
        let session = aether_core::Session {
            id: aether_core::SessionId::new("01J0000000000000000000T00").unwrap(),
            runtime_id: aether_core::RuntimeId::new("claude-code").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: "真相表".to_owned(),
            status: aether_core::SessionStatus::Idle,
            model: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            permission_mode: aether_core::PermissionMode::default(),
            preset_id: None,
            system_prompt: None,
            config: json!({}),
            token_usage: aether_core::TokenUsage::default(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
        };
        let provider_id = "01J0000000000000000000T01".to_owned();
        let providers = vec![
            aether_store::ProviderRecord {
                id: provider_id.clone(),
                name: "启用供应商".to_owned(),
                provider_type: "anthropic".to_owned(),
                base_url: Some("https://api.anthropic.com".to_owned()),
                endpoints: "{}".to_owned(),
                api_key_ref: None,
                enabled: true,
                is_builtin: false,
                created_at: 1,
                updated_at: 1,
            },
            aether_store::ProviderRecord {
                id: "01J0000000000000000000T02".to_owned(),
                name: "停用供应商".to_owned(),
                provider_type: "anthropic".to_owned(),
                base_url: None,
                endpoints: "{}".to_owned(),
                api_key_ref: None,
                enabled: false,
                is_builtin: false,
                created_at: 1,
                updated_at: 1,
            },
        ];
        let models = vec![
            aether_store::ProviderModelRecord {
                id: "01J0000000000000000000T03".to_owned(),
                provider_id,
                model_id: "claude-opus".to_owned(),
                display_name: "Opus".to_owned(),
                enabled: true,
                created_at: 1,
            },
            aether_store::ProviderModelRecord {
                id: "01J0000000000000000000T04".to_owned(),
                provider_id: "01J0000000000000000000T01".to_owned(),
                model_id: "claude-disabled".to_owned(),
                display_name: "Disabled".to_owned(),
                enabled: false,
                created_at: 1,
            },
        ];
        (session, providers, models)
    }

    fn with_provider(
        mut session: aether_core::Session,
        provider_id: Option<&str>,
    ) -> aether_core::Session {
        session.config = match provider_id {
            Some(provider_id) => json!({"aether": {"provider_id": provider_id}}),
            None => json!({}),
        };
        session
    }

    #[test]
    fn selection_truth_table_is_exhaustive_and_rejects_unlisted_combinations() {
        let (base, providers, models) = truth_table_fixture();
        let enabled = "01J0000000000000000000T01";
        let disabled = "01J0000000000000000000T02";
        let missing = "01J0000000000000000000T99";
        let expect_provider = |session, model: Option<&str>| {
            matches!(
                plan_session_selection(
                    &with_provider(session, None),
                    &providers,
                    &models,
                    Some(enabled),
                    model
                )
                .unwrap(),
                Some(SelectionPlan::Provider { .. })
            )
        };
        let expect_model = |session, model: &str| {
            matches!(
                plan_session_selection(&session, &providers, &models, None, Some(model)).unwrap(),
                Some(SelectionPlan::Model { .. })
            )
        };
        let error_of =
            |session: &aether_core::Session, provider: Option<&str>, model: Option<&str>| {
                plan_session_selection(session, &providers, &models, provider, model)
                    .unwrap_err()
                    .message
            };

        // 1：provider 空 + model 空 + send provider/model → 允许补选。
        assert!(expect_provider(base.clone(), Some("claude-opus")));
        // 2：provider 空 + model 空 + send provider only → 请选择模型。
        assert!(error_of(&base, Some(enabled), None).contains("请选择模型"));
        // 3/4：provider 空 + model 空 + 无 provider → 请选择供应商。
        assert!(error_of(&base, None, Some("claude-opus")).contains("请选择供应商"));
        assert!(error_of(&base, None, None).contains("请选择供应商"));
        // 5：provider 空 + model 有效 + send provider only → 允许沿用 model。
        let valid_model = aether_core::Session {
            model: Some("claude-opus".to_owned()),
            ..base.clone()
        };
        assert!(expect_provider(valid_model.clone(), None));
        // 6：provider 空 + model 有效 + send provider/model → 允许覆盖。
        assert!(expect_provider(valid_model.clone(), Some("claude-opus")));
        // 7/8：provider 空 + model 有效 + 无 provider → 请选择供应商。
        assert!(error_of(&valid_model, None, Some("claude-opus")).contains("请选择供应商"));
        assert!(error_of(&valid_model, None, None).contains("请选择供应商"));
        // 9：provider 空 + model 失效 + send provider/model → 允许。
        let invalid_model = aether_core::Session {
            model: Some("gone".to_owned()),
            ..base.clone()
        };
        assert!(expect_provider(invalid_model.clone(), Some("claude-opus")));
        // 10：provider 空 + model 失效 + send provider only → 请选择模型。
        assert!(error_of(&invalid_model, Some(enabled), None).contains("请选择模型"));
        // 11：provider 空 + model 失效 + 无 provider（任意 model）→ 请选择供应商。
        assert!(error_of(&invalid_model, None, Some("claude-opus")).contains("请选择供应商"));
        // 12：provider 有 + model 有效 + 无 send 字段 → 正常发送。
        let selected = with_provider(
            aether_core::Session {
                model: Some("claude-opus".to_owned()),
                ..base.clone()
            },
            Some(enabled),
        );
        assert!(
            plan_session_selection(&selected, &providers, &models, None, None)
                .unwrap()
                .is_none()
        );
        // 13：provider 有 + model 有效 + send provider → 禁止切换。
        assert!(error_of(&selected, Some(enabled), None).contains("禁止运行期切换供应商"));
        // 14：provider 有 + model 有效 + send model → 禁止改 model。
        assert!(error_of(&selected, None, Some("claude-opus")).contains("禁止运行期修改模型"));
        // 15：provider 有 + model 空 + send model → 允许一次性补选。
        let empty_model = with_provider(
            aether_core::Session {
                model: None,
                ..base.clone()
            },
            Some(enabled),
        );
        assert!(expect_model(empty_model.clone(), "claude-opus"));
        // 16：provider 有 + model 空 + 无 send model → 请选择模型。
        assert!(error_of(&empty_model, None, None).contains("请选择模型"));
        // 17：provider 有 + model 空 + send provider → 禁止切换。
        assert!(error_of(&empty_model, Some(enabled), Some("claude-opus"))
            .contains("禁止运行期切换供应商"));
        // 18：provider 有 + model 失效 + send model → 允许补选。
        let stale_model = with_provider(
            aether_core::Session {
                model: Some("claude-disabled".to_owned()),
                ..base.clone()
            },
            Some(enabled),
        );
        assert!(expect_model(stale_model.clone(), "claude-opus"));
        // 19：provider 有 + model 失效 + 无 send model → 请选择模型。
        assert!(error_of(&stale_model, None, None).contains("请选择模型"));
        // 20：provider 有 + model 失效 + send provider → 禁止切换。
        assert!(error_of(&stale_model, Some(enabled), Some("claude-opus"))
            .contains("禁止运行期切换供应商"));
        // 21：provider 停用/删除 → 只读（优先于并发检查）。
        let readonly = with_provider(
            aether_core::Session {
                model: Some("claude-opus".to_owned()),
                ..base.clone()
            },
            Some(disabled),
        );
        assert!(error_of(&readonly, None, None).contains("只读"));
        let deleted = with_provider(
            aether_core::Session {
                model: Some("claude-opus".to_owned()),
                ..base.clone()
            },
            Some(missing),
        );
        assert!(error_of(&deleted, None, Some("claude-opus")).contains("只读"));
        // 未列组合默认拒绝：provider 有 + 任意 send 字段组合均已覆盖；额外防护：
        // provider 空 + model 空 + send provider（model 非法）等由目标校验拒绝。
        let mut invalid_target = base.clone();
        invalid_target.model = None;
        // provider 空 + send provider/model（model 不属于）→ plan 允许，但目标校验拒绝。
        assert!(
            validate_selection_target(enabled, "claude-disabled", &providers, &models).is_err()
        );
    }

    #[test]
    fn provider_selection_state_handles_provider_empty_and_disabled_models() {
        let (_, providers, models) = truth_table_fixture();
        // provider 空 + model 在任一启用 provider 中存在且启用 → active。
        assert_eq!(
            provider_selection_state(None, Some("claude-opus"), &providers, &models).2,
            "active"
        );
        // provider 空 + model 停用 → invalid（不得判 active）。
        assert_eq!(
            provider_selection_state(None, Some("claude-disabled"), &providers, &models).2,
            "invalid"
        );
        // provider 空 + model 不存在 → invalid。
        assert_eq!(
            provider_selection_state(None, Some("gone"), &providers, &models).2,
            "invalid"
        );
        // provider 有效但 model 不属于 → invalid。
        assert_eq!(
            provider_selection_state(
                Some("01J0000000000000000000T01"),
                Some("gone"),
                &providers,
                &models
            )
            .2,
            "invalid"
        );
        // provider 失效 → invalid（UI 只读）。
        assert_eq!(
            provider_selection_state(
                Some("01J0000000000000000000T02"),
                Some("claude-opus"),
                &providers,
                &models
            )
            .1,
            "invalid"
        );
    }
}
