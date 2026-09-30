//! D7 命令面（MVP 全集）的请求 DTO。
//!
//! 全部使用 `#[serde(deny_unknown_fields)]`；可选项显式 `#[serde(default)]`。
//! 语义校验（长度 / 枚举 / 格式）在 `validate` 中完成，保持「解析 → 校验 → 下游」
//! 三段式与错误码稳定。

use serde::Deserialize;

use super::error::{IpcError, IpcErrorCode};
use super::path;
use super::validate::{
    self, ensure_max_bytes, ensure_max_chars, ensure_not_empty, ensure_value_size, is_ulid,
    reject_control_chars, CommandRequest, MAX_LABEL_CHARS, MAX_MESSAGE_BYTES, MAX_PAGE_LIMIT,
    MAX_PATH_CHARS, MAX_SETTING_VALUE_BYTES, MAX_TITLE_CHARS,
};

/// 权限决议（D9：`once` / `session` 授权；`deny` 拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Once,
    Session,
    Deny,
}

/// 会话状态过滤（与附录 C `sessions.status` CHECK 枚举一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Creating,
    Idle,
    Running,
    Paused,
    WaitingPermission,
    Completed,
    Failed,
    Cancelled,
}

/// 设置键白名单（默认拒绝）。
///
/// M1-08 尚无已批准设置项；后续里程碑在实现设置能力时，先在此登记键名并同步
/// `settings` 表语义，再放开对应命令。未登记键一律 `invalid_enum`。
/// M3-05 登记：`backup.reminder`（D13「7 天未备份提醒可开关」；布尔，缺省 `true`；
/// UI-UX Q9/Q10 裁定为全局开关）。工作区绑定键（M3-08）尚未登记。
pub const SETTINGS_KEY_ALLOWLIST: &[&str] = &["backup.reminder"];

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    pub runtime_id: String,
    pub title: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// 会话级思考深度（ADR-010 决策 2：0–4，缺省 2；越界 `out_of_range`、
    /// 类型非法 `invalid_type`）。
    #[serde(default)]
    pub thinking_depth: Option<i64>,
}

impl SessionCreateRequest {
    /// 请求的思考深度（已校验）；`None` = 缺省（核心按 2 应用）。
    pub fn thinking_depth_value(&self) -> Result<Option<u8>, IpcError> {
        thinking_depth_value(self.thinking_depth)
    }
}

impl CommandRequest for SessionCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate::validate_identifier(&self.runtime_id, "runtime_id")?;
        validate::ensure_not_empty(&self.title, "title")?;
        ensure_max_chars(&self.title, "title", MAX_TITLE_CHARS)?;
        reject_control_chars(&self.title, "title", false)?;
        if let Some(workspace_id) = &self.workspace_id {
            if !is_ulid(workspace_id) {
                return Err(IpcError::invalid_format(
                    "workspace_id",
                    "必须是 26 位 ULID",
                ));
            }
        }
        if let Some(model) = &self.model {
            validate::validate_model(model, "model")?;
        }
        if let Some(thinking_depth) = self.thinking_depth {
            validate::validate_thinking_depth(thinking_depth, "thinking_depth")?;
        }
        Ok(())
    }
}

/// 思考深度请求值 → `u8`（0–4；越界 `out_of_range`）。
///
/// DTO 以 `i64` 承载以区分错误类别：负数/越界 → `out_of_range`；
/// 浮点/字符串在 serde 反序列化阶段即 `invalid_type`（ADR-010 B.2 校验矩阵）。
pub fn thinking_depth_value(value: Option<i64>) -> Result<Option<u8>, IpcError> {
    match value {
        Some(value) => validate::validate_thinking_depth(value, "thinking_depth").map(Some),
        None => Ok(None),
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionListRequest {
    #[serde(default)]
    pub runtime_id: Option<String>,
    #[serde(default)]
    pub status: Option<SessionStatus>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl CommandRequest for SessionListRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(runtime_id) = &self.runtime_id {
            validate::validate_identifier(runtime_id, "runtime_id")?;
        }
        validate_page_limit(self.limit)
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionSendRequest {
    pub session_id: String,
    pub text: String,
    /// 幂等键（ADR-005）：必填 ULID；重复发送同一 `(session_id, client_msg_id)`
    /// 返回既有 message_id/run_id，核心重启后重放同样不重复。
    pub client_msg_id: String,
    /// 本次 run 的思考深度覆盖（ADR-010 决策 2：0–4；缺省 = 会话级值；
    /// 仅本次 run，不回写会话级）。
    #[serde(default)]
    pub thinking_depth: Option<i64>,
}

impl SessionSendRequest {
    /// 请求的思考深度覆盖（已校验）；`None` = 缺省（应用会话级值）。
    pub fn thinking_depth_value(&self) -> Result<Option<u8>, IpcError> {
        thinking_depth_value(self.thinking_depth)
    }
}

impl CommandRequest for SessionSendRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        ensure_max_bytes(&self.text, "text", MAX_MESSAGE_BYTES)?;
        reject_control_chars(&self.text, "text", true)?;
        if !is_ulid(&self.client_msg_id) {
            return Err(IpcError::invalid_format(
                "client_msg_id",
                "必须是 26 位 ULID（ADR-005 幂等键，必填）",
            ));
        }
        if let Some(thinking_depth) = self.thinking_depth {
            validate::validate_thinking_depth(thinking_depth, "thinking_depth")?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionIdRequest {
    pub session_id: String,
}

impl SessionIdRequest {
    fn validate_session_id(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

impl CommandRequest for SessionIdRequest {
    fn validate(&self) -> Result<(), IpcError> {
        self.validate_session_id()
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct MessagesPageRequest {
    pub session_id: String,
    #[serde(default)]
    pub last_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl CommandRequest for MessagesPageRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        validate_page_limit(self.limit)
    }
}

/// `ref_pick`（ADR-010 决策 1）：引用选择器 kind（文件 / 目录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RefPickKind {
    File,
    Directory,
}

/// `ref_pick`：`{ kind }` 严格解析（未知 kind → `invalid_enum`；未知成员拒绝）。
///
/// Rust 侧系统选择器（复用 `DirectoryPicker` 抽象并扩展文件选择；E2E 注入替身），
/// **不新增 WebView capability 权限面**（与 `startup_pick_target` 先例一致）；
/// 路径原样返回，不做 canonicalize（校验在 `artifact_add`，ADR-010 决策 1）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RefPickRequest {
    pub kind: RefPickKind,
}

impl CommandRequest for RefPickRequest {}

/// `artifacts_list`（ADR-010）：会话引用清单（按 `created_at` 升序）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsListRequest {
    pub session_id: String,
}

impl CommandRequest for ArtifactsListRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// `artifact_add`（ADR-010）：canonicalize + 可访问性检查在
/// [`path::validate_artifact_path`]（命令层执行；失败 `artifact_path_rejected`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ArtifactAddRequest {
    pub session_id: String,
    pub path: String,
}

impl ArtifactAddRequest {
    /// 引用路径的规范化与 kind/大小探测结果（命令层在调用后端前完成，失败即拒绝）。
    pub fn resolve_path(&self) -> Result<path::ArtifactPath, IpcError> {
        path::validate_artifact_path(&self.path)
    }
}

impl CommandRequest for ArtifactAddRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        ensure_not_empty(&self.path, "path")?;
        ensure_max_chars(&self.path, "path", MAX_PATH_CHARS)
    }
}

/// `artifact_remove`（ADR-010）：不存在 → 幂等 `{ removed: false }`（不新增错误码）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRemoveRequest {
    pub session_id: String,
    pub artifact_id: String,
}

impl CommandRequest for ArtifactRemoveRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        if !is_ulid(&self.artifact_id) {
            return Err(IpcError::invalid_format("artifact_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// 供应商类型（ADR-010 决策 3：应用层枚举校验，不加 CHECK；创建后不可改）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    Anthropic,
    Openai,
    Deepseek,
    Google,
    Custom,
}

/// 供应商名称上限（ADR-010 附录 B.1：≤128 字符）。
pub const MAX_PROVIDER_NAME_CHARS: usize = 128;
/// Base URL 上限（ADR-010 附录 B.1：≤2048 字符）。
pub const MAX_PROVIDER_BASE_URL_CHARS: usize = 2048;
/// API Key 明文上限（ADR-010 附录 B.1：非空 ≤8192 字符；仅传输，不落库）。
pub const MAX_API_KEY_CHARS: usize = 8192;

/// `providers_list`（ADR-010）：无参数命令；`null`/缺省/空对象合法，任何成员拒绝。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProvidersListRequest {}

impl CommandRequest for ProvidersListRequest {}

/// `provider_create`（ADR-010 决策 3）：`api_key` 明文仅传输（核心写 keyring）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderCreateRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    pub enabled: bool,
}

impl CommandRequest for ProviderCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_provider_name(&self.name)?;
        if let Some(base_url) = &self.base_url {
            if !base_url.is_empty() {
                validate_provider_base_url(base_url)?;
            }
        }
        if self.provider_type == ProviderType::Custom
            && !self.base_url.as_deref().is_some_and(|value| !value.is_empty())
        {
            return Err(IpcError::missing_field("base_url"));
        }
        if let Some(api_key) = &self.api_key {
            validate_api_key(api_key)?;
        }
        Ok(())
    }
}

/// `provider_update`（ADR-010 决策 3）：整体更新；`type` 不可改；
/// `api_key` 三态（缺省=不变、空串=清除、非空=覆盖）；`base_url` 缺省=不变、空串=清除。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderUpdateRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    pub enabled: bool,
}

impl CommandRequest for ProviderUpdateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.id) {
            return Err(IpcError::invalid_format("id", "必须是 26 位 ULID"));
        }
        validate_provider_name(&self.name)?;
        if let Some(base_url) = &self.base_url {
            if !base_url.is_empty() {
                validate_provider_base_url(base_url)?;
            }
        }
        if let Some(api_key) = &self.api_key {
            validate_api_key(api_key)?;
        }
        Ok(())
    }
}

/// `provider_delete`（ADR-010 决策 3）：内置拒绝（`builtin_provider_undeletable`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderDeleteRequest {
    pub id: String,
}

impl CommandRequest for ProviderDeleteRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.id) {
            return Err(IpcError::invalid_format("id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// `provider_toggle`（ADR-010 决策 3）：快速启用/停用（内置可停用）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderToggleRequest {
    pub id: String,
    pub enabled: bool,
}

impl CommandRequest for ProviderToggleRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.id) {
            return Err(IpcError::invalid_format("id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// `provider_model_add`（ADR-010 决策 3）：新增模型（默认启用）；
/// 重复 `(provider_id, model_id)` → `invalid_value`。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelAddRequest {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
}

impl CommandRequest for ProviderModelAddRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.provider_id) {
            return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
        }
        validate::validate_model(&self.model_id, "model_id")?;
        ensure_not_empty(&self.display_name, "display_name")?;
        ensure_max_chars(&self.display_name, "display_name", MAX_PROVIDER_NAME_CHARS)?;
        reject_control_chars(&self.display_name, "display_name", false)
    }
}

/// `provider_model_toggle`（ADR-010 决策 3）：模型启用/停用；不存在 →
/// `provider_model_not_found`。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelToggleRequest {
    pub provider_id: String,
    pub model_id: String,
    pub enabled: bool,
}

impl CommandRequest for ProviderModelToggleRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.provider_id) {
            return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
        }
        validate::validate_model(&self.model_id, "model_id")
    }
}

fn validate_provider_name(name: &str) -> Result<(), IpcError> {
    ensure_not_empty(name, "name")?;
    ensure_max_chars(name, "name", MAX_PROVIDER_NAME_CHARS)?;
    reject_control_chars(name, "name", false)
}

/// Base URL 格式（ADR-010 附录 B.1：`https?://` 前缀、≤2048 字符）。
fn validate_provider_base_url(base_url: &str) -> Result<(), IpcError> {
    ensure_max_chars(base_url, "base_url", MAX_PROVIDER_BASE_URL_CHARS)?;
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(IpcError::invalid_format(
            "base_url",
            "必须以 http:// 或 https:// 开头",
        ));
    }
    reject_control_chars(base_url, "base_url", false)
}

/// API Key 明文（仅传输）：非空时 ≤8192 字符（`too_large`）；拒绝控制字符。
fn validate_api_key(api_key: &str) -> Result<(), IpcError> {
    if api_key.is_empty() {
        return Ok(());
    }
    ensure_max_chars(api_key, "api_key", MAX_API_KEY_CHARS)?;
    reject_control_chars(api_key, "api_key", false)
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PermissionsPendingRequest {
    #[serde(default)]
    pub session_id: Option<String>,
}

impl CommandRequest for PermissionsPendingRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(session_id) = &self.session_id {
            if !is_ulid(session_id) {
                return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PermissionResolveRequest {
    pub request_id: String,
    pub decision: PermissionDecision,
}

impl CommandRequest for PermissionResolveRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.request_id) {
            return Err(IpcError::invalid_format("request_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SettingsGetRequest {
    pub key: String,
}

impl CommandRequest for SettingsGetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_settings_key(&self.key)
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SettingsSetRequest {
    pub key: String,
    pub value: crate::json_payload::JsonPayload,
}

impl CommandRequest for SettingsSetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_settings_key(&self.key)?;
        ensure_value_size(&self.value.0, "value", MAX_SETTING_VALUE_BYTES)
    }
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct BackupCreateRequest {
    #[serde(default)]
    pub label: Option<String>,
    /// 外部目标目录（D13：经系统目录选择器选择；缺省 = 应用 `backups` 目录）。
    /// 命令层 canonicalize（绝对路径 + 存在目录）后传入后端；空间校验在后端执行。
    #[serde(default)]
    pub target_dir: Option<String>,
}

impl BackupCreateRequest {
    /// 外部目标目录的规范化结果（缺省来源为 `None`）。
    pub fn canonical_target_dir(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.target_dir {
            Some(target_dir) => path::validate_backup_target_dir(target_dir).map(Some),
            None => Ok(None),
        }
    }
}

impl CommandRequest for BackupCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(label) = &self.label {
            ensure_not_empty(label, "label")?;
            ensure_max_chars(label, "label", MAX_LABEL_CHARS)?;
            reject_control_chars(label, "label", false)?;
        }
        if let Some(target_dir) = &self.target_dir {
            ensure_not_empty(target_dir, "target_dir")?;
        }
        Ok(())
    }
}

/// `backup_list`（ADR-004）：无参数命令；非空成员一律拒绝。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct BackupListRequest {}

impl CommandRequest for BackupListRequest {}

/// `health`（ADR-007 决策 1）：无参数命令（严格解析：任何成员拒绝）。
///
/// 仅本地 IPC 查询：不落库、不产生事件；返回 `HealthReport`
/// （`storage_state` / `write_queue_depth` / `runtimes` 摘要 / `ts`）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct HealthRequest {}

impl CommandRequest for HealthRequest {}

/// 备份来源（ADR-004 `backup_restore`）：内部备份 id 枚举 或 外部 `.db` 路径。
#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BackupSource {
    /// 内部备份 id（`backups` 表白名单，仅 ULID 格式层校验；存在性由后端判定）。
    Internal { id: String },
    /// 外部 `.db` 路径（canonicalize + 存在性 + 后缀，D13 七步第一步）。
    External { path: String },
}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct BackupRestoreRequest {
    pub source: BackupSource,
}

impl BackupRestoreRequest {
    /// 外部候选路径的规范化结果（内部来源为 `None`）。
    pub fn canonical_external_path(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.source {
            BackupSource::Internal { .. } => Ok(None),
            BackupSource::External { path } => path::validate_external_file(path, "db").map(Some),
        }
    }
}

impl CommandRequest for BackupRestoreRequest {
    fn validate(&self) -> Result<(), IpcError> {
        match &self.source {
            BackupSource::Internal { id } => {
                if !is_ulid(id) {
                    return Err(IpcError::invalid_format(
                        "source.internal.id",
                        "必须是 26 位 ULID（内部备份 id）",
                    ));
                }
                Ok(())
            }
            BackupSource::External { path } => path::validate_external_file(path, "db").map(|_| ()),
        }
    }
}

/// `app_restart`（ADR-004）：显式 `confirm:true` 才允许复用关闭序列重启。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AppRestartRequest {
    pub confirm: bool,
}

impl CommandRequest for AppRestartRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !self.confirm {
            return Err(IpcError::at_field(
                IpcErrorCode::InvalidValue,
                "confirm",
                "必须显式 confirm:true（重启将中断全部在途 run）",
            ));
        }
        Ok(())
    }
}

/// `app_exit`（ADR-006 决策 1，v0.3 对齐）：拒绝启动页退出；显式 `confirm:true`
/// 才允许退出（与 `app_restart` 的 confirm 约定一致）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AppExitRequest {
    pub confirm: bool,
}

impl CommandRequest for AppExitRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !self.confirm {
            return Err(IpcError::at_field(
                IpcErrorCode::InvalidValue,
                "confirm",
                "必须显式 confirm:true（与 app_restart 的 confirm 约定一致）",
            ));
        }
        Ok(())
    }
}

/// `run_retry`（ADR-004）：仅终态 run 可重试；格式层校验 ULID，终态由后端判定。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RunRetryRequest {
    pub run_id: String,
}

impl CommandRequest for RunRetryRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.run_id) {
            return Err(IpcError::invalid_format("run_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// `runtime_retry`（ADR-004/M1-10）：仅 `disabled + start_failed` 可用；
/// `runtime_id` 必须命中 `runtimes` 白名单（存在性由后端判定）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRetryRequest {
    pub runtime_id: String,
}

impl CommandRequest for RuntimeRetryRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate::validate_identifier(&self.runtime_id, "runtime_id")
    }
}

/// `runtime_enable`（ADR-004/M1-10）：仅 `disabled` 可用；
/// `untrusted` / `version_mismatch` 必须先修复后再启用（后端状态判定）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEnableRequest {
    pub runtime_id: String,
}

impl CommandRequest for RuntimeEnableRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate::validate_identifier(&self.runtime_id, "runtime_id")
    }
}

/// `workspace_set`（ADR-004/D14）：`workspace_id` 存在性 或 `root_path`
/// canonicalize（目录须存在；命中同步盘拒绝清单则拒绝）。P0 仅对新会话生效。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSetRequest {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub root_path: Option<String>,
}

impl WorkspaceSetRequest {
    /// 目标工作区根目录的规范化结果（按 `workspace_id` 绑定时为 `None`，由后端解析）。
    pub fn canonical_root_path(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.root_path {
            Some(root_path) => path::validate_workspace_root(root_path).map(Some),
            None => Ok(None),
        }
    }
}

impl CommandRequest for WorkspaceSetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        match (&self.workspace_id, &self.root_path) {
            (Some(workspace_id), None) => {
                if !is_ulid(workspace_id) {
                    return Err(IpcError::invalid_format(
                        "workspace_id",
                        "必须是 26 位 ULID",
                    ));
                }
                Ok(())
            }
            (None, Some(root_path)) => path::validate_workspace_root(root_path).map(|_| ()),
            _ => Err(IpcError::invalid_value(
                "必须且只能提供 workspace_id 或 root_path 之一",
            )),
        }
    }
}

/// `startup_migrate`（M1-06/A4）：迁移目标目录。
///
/// 形态校验（绝对路径、存在目录、Windows 特殊路径）在命令内完成；目标自身的 A4
/// 同步盘复核在启动门迁移流内执行（同一检测上下文）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StartupMigrateRequest {
    pub target_dir: String,
}

impl CommandRequest for StartupMigrateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        ensure_not_empty(&self.target_dir, "target_dir")
    }
}

/// `startup_get`（ADR-006）：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StartupGetRequest {}

impl CommandRequest for StartupGetRequest {}

/// `startup_pick_target`（ADR-006）：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StartupPickTargetRequest {}

impl CommandRequest for StartupPickTargetRequest {}

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ExportDiagnosticsRequest {
    pub target_dir: String,
}

impl ExportDiagnosticsRequest {
    /// 导出目标目录的规范化结果（ADR-003 决策 19：导出到外部路径；经系统选择器选择，
    /// 不做默认目录信任；canonicalize + 存在目录，空间护栏在后端执行）。
    pub fn canonical_target_dir(&self) -> Result<std::path::PathBuf, IpcError> {
        path::validate_backup_target_dir(&self.target_dir)
    }
}

impl CommandRequest for ExportDiagnosticsRequest {
    fn validate(&self) -> Result<(), IpcError> {
        ensure_not_empty(&self.target_dir, "target_dir")
    }
}

fn validate_page_limit(limit: Option<u32>) -> Result<(), IpcError> {
    match limit {
        Some(0) => Err(IpcError::out_of_range(
            "limit",
            format!("必须在 1..={MAX_PAGE_LIMIT} 之间"),
        )),
        Some(limit) if limit > MAX_PAGE_LIMIT => Err(IpcError::out_of_range(
            "limit",
            format!("分页上限 {MAX_PAGE_LIMIT}（设计 D7）"),
        )),
        _ => Ok(()),
    }
}

fn validate_settings_key(key: &str) -> Result<(), IpcError> {
    validate::ensure_not_empty(key, "key")?;
    ensure_max_chars(key, "key", 64)?;
    if !SETTINGS_KEY_ALLOWLIST.contains(&key) {
        return Err(IpcError::invalid_enum(
            "key",
            format!("未登记的设置键（当前白名单：{SETTINGS_KEY_ALLOWLIST:?}）"),
        ));
    }
    Ok(())
}
