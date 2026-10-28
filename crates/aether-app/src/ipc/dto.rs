//! D7 命令面（MVP 全集）的请求 DTO。
//!
//! 全部使用 `#[serde(deny_unknown_fields)]`；可选项显式 `#[serde(default)]`。
//! 语义校验（长度 / 枚举 / 格式）在 `validate` 中完成，保持「解析 → 校验 → 下游」
//! 三段式与错误码稳定。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::cursor;
use super::error::{IpcError, IpcErrorCode};
use super::path;
use super::validate::{
    self, ensure_max_bytes, ensure_max_chars, ensure_not_empty, ensure_value_size, is_ulid,
    reject_control_chars, CommandRequest, MAX_LABEL_CHARS, MAX_MESSAGE_BYTES, MAX_PAGE_LIMIT,
    MAX_PASSPHRASE_CHARS, MAX_PATH_CHARS, MAX_SETTING_VALUE_BYTES, MAX_TITLE_CHARS,
};

/// 权限决议（D9；M6-01/ADR-019 启用 `always` 持久授权「记住」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Once,
    Session,
    Always,
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
/// UI-UX Q9/Q10 裁定为全局开关）。
/// M5-01/P1 批次登记（ADR-018 附录 A-6；登记先行，功能随各 P1 任务落地）：
/// `backup.auto.enabled` / `backup.auto.frequency` / `backup.auto.retention` /
/// `backup.encryption.enabled`（ADR-020 / M5-02、M5-03）、`events.retention.enabled` /
/// `events.retention.days`（ADR-021 / M5-04）、`ui.layout`（M7-04 布局持久化落点）、
/// `ui.provider_selection`（ADR-028 v0.9 全局供应商记忆：JSON 对象字符串 ≤64KiB）。
/// 键名/类型/缺省值/语义/迁移影响见 ADR-018 附录 A-6。
pub const SETTINGS_KEY_ALLOWLIST: &[&str] = &[
    "backup.reminder",
    "backup.auto.enabled",
    "backup.auto.frequency",
    "backup.auto.retention",
    "backup.encryption.enabled",
    "events.retention.enabled",
    "events.retention.days",
    "ui.layout",
    "ui.provider_selection",
];

#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    /// 运行时 id（M7-02/ADR-022：由必填改**可选**——缺省 = 自动路由；显式 = 手动指定，
    /// 优先于自动路由且不参与打分；仍须通过可用性校验：未注册 → `invalid_enum`，
    /// 已注册但 `enabled=0`/`disabled` → `invalid_value`）。
    #[serde(default)]
    pub runtime_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// ADR-028 v0.9：会话级模型（三官方运行时语义必填；缺失 →
    /// `invalid_value("请选择模型")`；旧会话 `sessions.model` 空/失效/不属于时可一次性
    /// `session_send.model` 补选）。
    #[serde(default)]
    pub model: Option<String>,
    /// ADR-028：会话级供应商 id（可选；`None` = 由模型解析/旧单供应商路径）。
    /// 显式指定时校验供应商存在/启用、模型归属与运行时协议端点。
    #[serde(default)]
    pub provider_id: Option<String>,
    /// 会话级思考深度（ADR-010 决策 2：0–4，缺省 2；越界 `out_of_range`、
    /// 类型非法 `invalid_type`）。
    #[serde(default)]
    pub thinking_depth: Option<i64>,
    /// 会话级权限模式（M6-01/ADR-014 决策 3：`read_only` / `workspace_write`（缺省）/
    /// `danger_full_access`；非法枚举 → `invalid_enum`）。
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Agent 预设（M7-09/ADR-026 §2.2：可选；ULID；查无 → `preset_not_found`）。
    /// 合成结果落 `sessions.system_prompt` 并经 `session.create` 下发适配器。
    #[serde(default)]
    pub preset_id: Option<String>,
}

impl SessionCreateRequest {
    /// 请求的思考深度（已校验）；`None` = 缺省（核心按 2 应用）。
    pub fn thinking_depth_value(&self) -> Result<Option<u8>, IpcError> {
        thinking_depth_value(self.thinking_depth)
    }

    /// 请求的权限模式（已校验）；`None` = 缺省 `workspace_write`。
    pub fn permission_mode_value(&self) -> Result<Option<aether_core::PermissionMode>, IpcError> {
        permission_mode_value(self.permission_mode.as_deref())
    }
}

impl CommandRequest for SessionCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(runtime_id) = &self.runtime_id {
            validate::validate_identifier(runtime_id, "runtime_id")?;
        }
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
        if let Some(provider_id) = &self.provider_id {
            if !is_ulid(provider_id) {
                return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
            }
        }
        if let Some(thinking_depth) = self.thinking_depth {
            validate::validate_thinking_depth(thinking_depth, "thinking_depth")?;
        }
        self.permission_mode_value()?;
        if let Some(preset_id) = &self.preset_id {
            if !is_ulid(preset_id) {
                return Err(IpcError::invalid_format("preset_id", "必须是 26 位 ULID"));
            }
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

/// 权限模式请求值 → [`aether_core::PermissionMode`]（三档枚举；非法值 → `invalid_enum`）。
pub fn permission_mode_value(
    value: Option<&str>,
) -> Result<Option<aether_core::PermissionMode>, IpcError> {
    match value {
        Some(value) => aether_core::PermissionMode::from_code(value)
            .map(Some)
            .ok_or_else(|| {
                IpcError::invalid_enum(
                    "permission_mode",
                    format!(
                        "权限模式非法（{value}）；允许值：read_only / workspace_write / danger_full_access"
                    ),
                )
            }),
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

/// 会话搜索查询上限（ADR-025 §2.1-2：≤256 字符；超限 `too_large`）。
pub const MAX_SESSION_SEARCH_CHARS: usize = 256;
/// 会话搜索分页上限（ADR-025 §2.1-2：`limit` ≤100，缺省 20）。
pub const SESSION_SEARCH_MAX_LIMIT: u32 = 100;
/// 会话搜索缺省分页（ADR-025 §2.1-2）。
pub const SESSION_SEARCH_DEFAULT_LIMIT: u32 = 20;

/// `session_search`（M7-07/ADR-025 §2.1-2）：标题 + 消息正文 FTS5 检索。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u64>,
}

impl SessionSearchRequest {
    /// 校验后的 limit（缺省 20；1..=100）。
    pub fn limit_value(&self) -> Result<u32, IpcError> {
        let limit = self.limit.unwrap_or(SESSION_SEARCH_DEFAULT_LIMIT);
        if limit == 0 {
            return Err(IpcError::out_of_range(
                "limit",
                "必须在 1..=100 之间（ADR-025）",
            ));
        }
        if limit > SESSION_SEARCH_MAX_LIMIT {
            return Err(IpcError::out_of_range(
                "limit",
                format!("会话搜索分页上限 {SESSION_SEARCH_MAX_LIMIT}（ADR-025）"),
            ));
        }
        Ok(limit)
    }

    /// FTS5 短语查询文本（用户输入按短语处理；不直接拼接 FTS 语法）。
    pub fn phrase(&self) -> String {
        format!("\"{}\"", self.query.trim().replace('"', "\"\""))
    }
}

impl CommandRequest for SessionSearchRequest {
    fn validate(&self) -> Result<(), IpcError> {
        let query = self.query.trim();
        if query.is_empty() {
            return Err(IpcError::invalid_value("搜索关键词不能为空或纯空白"));
        }
        if query.chars().count() > MAX_SESSION_SEARCH_CHARS {
            return Err(IpcError::too_large(
                "query",
                format!("搜索关键词上限 {MAX_SESSION_SEARCH_CHARS} 字符（ADR-025）"),
            ));
        }
        self.limit_value()?;
        Ok(())
    }
}

/// `session_rename`（M7-07/ADR-025 §2.1-3）：会话改名。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameRequest {
    pub session_id: String,
    pub title: String,
}

impl CommandRequest for SessionRenameRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        validate::ensure_not_empty(&self.title, "title")?;
        if self.title.trim().is_empty() {
            return Err(IpcError::invalid_value("标题不能为空或纯空白"));
        }
        ensure_max_chars(&self.title, "title", MAX_TITLE_CHARS)?;
        reject_control_chars(&self.title, "title", false)?;
        Ok(())
    }
}

/// `session_delete`（M7-07/ADR-025 §2.1-4）：硬删除会话（UI 二次确认 + 不可恢复提示）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteRequest {
    pub session_id: String,
}

impl CommandRequest for SessionDeleteRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        Ok(())
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
    /// 随消息附带的文件引用（M7-06/ADR-024：≤32 条；canonicalize + 可访问性校验，
    /// 失败 `artifact_path_rejected`；**引用 ≠ 预授权**，读取仍走 `fs.read` 权限门）。
    #[serde(default)]
    pub references: Vec<ReferenceEntry>,
    /// ADR-028 v0.9：旧会话补选供应商（仅 `sessions.config.aether.provider_id` 为空时
    /// 生效；已选会话再传 → `invalid_value`）。不新增命令。
    #[serde(default)]
    pub provider_id: Option<String>,
    /// ADR-028 v0.9：旧会话补选模型（`sessions.model` 空/失效/不属于时允许一次性补选；
    /// 已选且有效再传 → `invalid_value`）。
    #[serde(default)]
    pub model: Option<String>,
}

/// 引用条目（D6 `session.send.references` 载荷形态；ADR-024 附录 A-3）。
#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ReferenceEntry {
    pub path: String,
    /// `file` / `directory`（未知值 → `invalid_enum`）。
    pub kind: String,
}

/// 单次 `session.send` 引用条数上限（ADR-024 §2.1-5：≤32 条）。
pub const MAX_SESSION_REFERENCES: usize = 32;

impl SessionSendRequest {
    /// 请求的思考深度覆盖（已校验）；`None` = 缺省（应用会话级值）。
    pub fn thinking_depth_value(&self) -> Result<Option<u8>, IpcError> {
        thinking_depth_value(self.thinking_depth)
    }

    /// 解析引用条目为内核形态（canonicalize 后再取一次；与 `validate` 同口径）。
    ///
    /// 返回的顺序与请求一致；`kind` 使用请求声明的值（探测到的文件/目录形态仅作
    /// 可访问性校验，不覆盖用户声明语义）。
    pub fn resolved_references(&self) -> Result<Vec<aether_core::MessageReference>, IpcError> {
        let mut resolved = Vec::with_capacity(self.references.len());
        for entry in &self.references {
            let artifact = path::validate_artifact_path(&entry.path)?;
            let kind = aether_core::ReferenceKind::from_code(&entry.kind).ok_or_else(|| {
                IpcError::invalid_enum(
                    "references[].kind",
                    format!("引用类型非法（{}）；合法值：file / directory", entry.kind),
                )
            })?;
            resolved.push(aether_core::MessageReference {
                path: artifact.canonical.to_string_lossy().into_owned(),
                kind,
            });
        }
        Ok(resolved)
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
        if let Some(provider_id) = &self.provider_id {
            if !is_ulid(provider_id) {
                return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
            }
        }
        if let Some(model) = &self.model {
            validate::validate_model(model, "model")?;
        }
        if self.references.len() > MAX_SESSION_REFERENCES {
            return Err(IpcError::too_large(
                "references",
                format!(
                    "引用条数超过上限 {MAX_SESSION_REFERENCES}（当前 {}）",
                    self.references.len()
                ),
            ));
        }
        for entry in &self.references {
            if entry.kind.is_empty() || aether_core::ReferenceKind::from_code(&entry.kind).is_none()
            {
                return Err(IpcError::invalid_enum(
                    "references[].kind",
                    format!("引用类型非法（{}）；合法值：file / directory", entry.kind),
                ));
            }
            // 路径 canonicalize + 可访问性校验（失败 `artifact_path_rejected`）。
            path::validate_artifact_path(&entry.path)?;
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
    /// 响应预算（ADR-009 §9：64KiB–1.5MiB，缺省 1MiB）。
    #[serde(default)]
    pub max_bytes: Option<u32>,
    /// 不透明游标（仅 events 补读流；与 `last_seq` 互斥）。
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl CommandRequest for MessagesPageRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        validate_page_limit(self.limit)?;
        cursor::validate_max_bytes(self.max_bytes, "max_bytes")?;
        if let Some(token) = &self.next_cursor {
            if self.last_seq.is_some() {
                return Err(IpcError::at_field(
                    IpcErrorCode::InvalidValue,
                    "next_cursor",
                    "next_cursor 与 last_seq 互斥（游标已包含续读位置）",
                ));
            }
            cursor::validate_cursor(token, cursor::STREAM_EVENTS)?;
        }
        Ok(())
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
    /// ADR-028：协议端点（`anthropic`/`openai`/`google`/`dsh` → base URL）。
    #[serde(default)]
    pub endpoints: Option<BTreeMap<String, String>>,
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
            && !self
                .base_url
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        {
            return Err(IpcError::missing_field("base_url"));
        }
        if let Some(api_key) = &self.api_key {
            validate_api_key(api_key)?;
        }
        if let Some(endpoints) = &self.endpoints {
            validate_provider_endpoints(endpoints)?;
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
    /// ADR-028：`None` = 不变；`Some(map)` = 整体替换（`{}` 清除）。
    #[serde(default)]
    pub endpoints: Option<BTreeMap<String, String>>,
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
        if let Some(endpoints) = &self.endpoints {
            validate_provider_endpoints(endpoints)?;
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

/// M7-06/ADR-024：供应商连通性测试（`provider_test`；`{ provider_id }`）。
///
/// 网络请求参数（超时/端点/错误码）在命令后端实现；本层只做参数校验。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderTestRequest {
    pub provider_id: String,
}

impl CommandRequest for ProviderTestRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.provider_id) {
            return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// M7-06/ADR-024：从供应商获取模型列表（`provider_models_fetch`；`{ provider_id }`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelsFetchRequest {
    pub provider_id: String,
}

impl CommandRequest for ProviderModelsFetchRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.provider_id) {
            return Err(IpcError::invalid_format("provider_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
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

/// ADR-028：协议端点映射校验（仅允许已知协议；值必须为 HTTP(S) URL）。
fn validate_provider_endpoints(endpoints: &BTreeMap<String, String>) -> Result<(), IpcError> {
    if endpoints.len() > 8 {
        return Err(IpcError::invalid_value("endpoints 最多 8 项"));
    }
    for (protocol, url) in endpoints {
        if !matches!(protocol.as_str(), "anthropic" | "openai" | "google" | "dsh") {
            return Err(IpcError::invalid_enum(
                "endpoints",
                format!("未知协议端点：{protocol}"),
            ));
        }
        if url.trim().is_empty() {
            continue;
        }
        ensure_max_chars(url, "endpoints", MAX_PROVIDER_BASE_URL_CHARS)?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(IpcError::invalid_format(
                "endpoints",
                format!("协议 {protocol} 的端点必须以 http:// 或 https:// 开头"),
            ));
        }
        reject_control_chars(url, "endpoints", false)?;
    }
    Ok(())
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

/// M6-01/ADR-014 决策 3：会话级权限模式设置（`session_permission_mode_set`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionPermissionModeSetRequest {
    pub session_id: String,
    pub permission_mode: String,
}

impl SessionPermissionModeSetRequest {
    /// 请求的权限模式（三档枚举；非法值 → `invalid_enum`）。
    pub fn permission_mode_value(&self) -> Result<aether_core::PermissionMode, IpcError> {
        permission_mode_value(Some(&self.permission_mode))?.ok_or_else(|| {
            IpcError::invalid_enum("permission_mode", "权限模式不得为空（三档枚举必填）")
        })
    }
}

impl CommandRequest for SessionPermissionModeSetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        self.permission_mode_value()?;
        Ok(())
    }
}

/// M6-01/ADR-019：规则名上限（ADR-019 附录 A-1：≤64 字符）。
pub const MAX_RULE_NAME_CHARS: usize = 64;

/// M6-01/ADR-019：权限规则目标长度上限（path 类按路径上限）。
pub const MAX_RULE_TARGET_CHARS: usize = MAX_PATH_CHARS;

/// 规则资源白名单（`permission_rules.resource` CHECK 枚举）。
pub const RULE_RESOURCES: &[&str] = &["fs.read", "fs.write", "exec", "net"];
/// 规则动作白名单（`permission_rules.action` CHECK 枚举）。
pub const RULE_ACTIONS: &[&str] = &["read", "write", "exec", "connect"];
/// 规则目标类型白名单（`permission_rules.target_kind` CHECK 枚举）。
pub const RULE_TARGET_KINDS: &[&str] = &["path", "command", "host", "any"];
/// 规则决议白名单（`permission_rules.decision` CHECK 枚举；规则不产生 `ask`）。
pub const RULE_DECISIONS: &[&str] = &["allow", "deny"];

/// 规则 `resource × target_kind` 兼容矩阵（ADR-019 附录 A-1 硬口径；非法组合 → `invalid_enum`）。
pub fn rule_target_kind_allowed(resource: &str, target_kind: &str) -> bool {
    match resource {
        "fs.read" | "fs.write" => matches!(target_kind, "path" | "any"),
        "exec" => matches!(target_kind, "command" | "any"),
        "net" => matches!(target_kind, "host" | "any"),
        _ => false,
    }
}

fn ensure_enum(value: &str, field: &str, allowed: &[&str]) -> Result<(), IpcError> {
    if !allowed.contains(&value) {
        return Err(IpcError::invalid_enum(
            field,
            format!("取值非法（{value}）；允许值：{}", allowed.join(" / ")),
        ));
    }
    Ok(())
}

fn validate_rule_name(value: &str) -> Result<(), IpcError> {
    ensure_not_empty(value, "name")?;
    ensure_max_chars(value, "name", MAX_RULE_NAME_CHARS)?;
    reject_control_chars(value, "name", false)
}

fn validate_rule_target_field(value: &str) -> Result<(), IpcError> {
    ensure_max_chars(value, "target", MAX_RULE_TARGET_CHARS)
}

/// M6-01/ADR-018：`rules_list`（无参数）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RulesListRequest {}

impl CommandRequest for RulesListRequest {}

/// M6-01/ADR-019：`rule_add` 参数（`{ name, resource, action, target_kind, target?, decision, priority? }`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuleAddRequest {
    pub name: String,
    pub resource: String,
    pub action: String,
    pub target_kind: String,
    #[serde(default)]
    pub target: Option<String>,
    pub decision: String,
    #[serde(default)]
    pub priority: Option<i64>,
}

impl RuleAddRequest {
    pub fn priority_value(&self) -> Result<i64, IpcError> {
        match self.priority {
            Some(value) if !(0..=100).contains(&value) => Err(IpcError::out_of_range(
                "priority",
                format!("priority 必须在 0..=100 之间，传入 {value}"),
            )),
            Some(value) => Ok(value),
            None => Ok(0),
        }
    }
}

impl CommandRequest for RuleAddRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_rule_name(&self.name)?;
        ensure_enum(&self.resource, "resource", RULE_RESOURCES)?;
        ensure_enum(&self.action, "action", RULE_ACTIONS)?;
        ensure_enum(&self.target_kind, "target_kind", RULE_TARGET_KINDS)?;
        ensure_enum(&self.decision, "decision", RULE_DECISIONS)?;
        if !rule_target_kind_allowed(&self.resource, &self.target_kind) {
            return Err(IpcError::invalid_enum(
                "target_kind",
                format!(
                    "resource={} 不接受 target_kind={}（ADR-019 兼容矩阵）",
                    self.resource, self.target_kind
                ),
            ));
        }
        if let Some(target) = &self.target {
            validate_rule_target_field(target)?;
        }
        self.priority_value()?;
        Ok(())
    }
}

/// M6-01/ADR-019：`rule_update` 参数（至少 1 个变更字段；`rule_id` 存在性由后端判定）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuleUpdateRequest {
    pub rule_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub target_kind: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
}

impl RuleUpdateRequest {
    /// 是否至少提供 1 个变更字段。
    pub fn has_changes(&self) -> bool {
        self.name.is_some()
            || self.resource.is_some()
            || self.action.is_some()
            || self.target_kind.is_some()
            || self.target.is_some()
            || self.decision.is_some()
            || self.priority.is_some()
    }
}

impl CommandRequest for RuleUpdateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.rule_id) {
            return Err(IpcError::invalid_format("rule_id", "必须是 26 位 ULID"));
        }
        if !self.has_changes() {
            return Err(IpcError::invalid_value(
                "rule_update 至少需要 1 个变更字段（name/resource/action/target_kind/target/decision/priority）",
            ));
        }
        if let Some(name) = &self.name {
            validate_rule_name(name)?;
        }
        if let Some(resource) = &self.resource {
            ensure_enum(resource, "resource", RULE_RESOURCES)?;
        }
        if let Some(action) = &self.action {
            ensure_enum(action, "action", RULE_ACTIONS)?;
        }
        if let Some(target_kind) = &self.target_kind {
            ensure_enum(target_kind, "target_kind", RULE_TARGET_KINDS)?;
        }
        if let Some(decision) = &self.decision {
            ensure_enum(decision, "decision", RULE_DECISIONS)?;
        }
        if let Some(target) = &self.target {
            validate_rule_target_field(target)?;
        }
        if let Some(value) = self.priority {
            if !(0..=100).contains(&value) {
                return Err(IpcError::out_of_range(
                    "priority",
                    format!("priority 必须在 0..=100 之间，传入 {value}"),
                ));
            }
        }
        Ok(())
    }
}

/// M6-01/ADR-019：`rule_delete` 参数。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuleDeleteRequest {
    pub rule_id: String,
}

impl CommandRequest for RuleDeleteRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.rule_id) {
            return Err(IpcError::invalid_format("rule_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

/// M6-01/ADR-019：`rule_toggle` 参数。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuleToggleRequest {
    pub rule_id: String,
    pub enabled: bool,
}

impl CommandRequest for RuleToggleRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.rule_id) {
            return Err(IpcError::invalid_format("rule_id", "必须是 26 位 ULID"));
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
    /// M5-03/ADR-020：口令加密模式（显式可选；仅传输，不落盘、不入日志/诊断/导出）。
    #[serde(default)]
    pub passphrase: Option<String>,
}

impl BackupCreateRequest {
    /// 外部目标目录的规范化结果（缺省来源为 `None`）。
    pub fn canonical_target_dir(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.target_dir {
            Some(target_dir) => path::validate_backup_target_dir(target_dir).map(Some),
            None => Ok(None),
        }
    }

    /// 口令（已校验）包装（M5-03；`None` = 缺省主密钥模式/未加密）。
    pub fn passphrase_value(&self) -> Option<aether_security::SecretValue> {
        self.passphrase
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(aether_security::SecretValue::new)
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
        if let Some(passphrase) = &self.passphrase {
            ensure_not_empty(passphrase, "passphrase")?;
            ensure_max_chars(passphrase, "passphrase", MAX_PASSPHRASE_CHARS)?;
            reject_control_chars(passphrase, "passphrase", false)?;
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
    /// M5-03/ADR-020：口令模式产物的恢复口令（显式可选；仅传输，不落盘、不入日志）。
    #[serde(default)]
    pub passphrase: Option<String>,
}

impl BackupRestoreRequest {
    /// 外部候选路径的规范化结果（内部来源为 `None`）。
    ///
    /// M5-03：候选后缀接受 `.db`（P0 明文产物）或 `.aebk`（`AEBK1` 加密产物）。
    pub fn canonical_external_path(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.source {
            BackupSource::Internal { .. } => Ok(None),
            BackupSource::External { path } => {
                path::validate_external_backup_candidate(path).map(Some)
            }
        }
    }

    /// 口令（已校验）包装（M5-03；`None` = 未提供）。
    pub fn passphrase_value(&self) -> Option<aether_security::SecretValue> {
        self.passphrase
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(aether_security::SecretValue::new)
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
            }
            BackupSource::External { path } => {
                path::validate_external_backup_candidate(path).map(|_| ())?;
            }
        }
        if let Some(passphrase) = &self.passphrase {
            ensure_not_empty(passphrase, "passphrase")?;
            ensure_max_chars(passphrase, "passphrase", MAX_PASSPHRASE_CHARS)?;
            reject_control_chars(passphrase, "passphrase", false)?;
        }
        Ok(())
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

/// `runtime_disable`（M7-01/ADR-015）：`runtime_id` 命中注册表（存在性由后端判定）；
/// 存在在途会话且未带 `confirm_terminate: true` → `runtime_busy`（不转移、不写偏好）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDisableRequest {
    pub runtime_id: String,
    /// 显式确认终止在途会话（缺省 `false`）。
    #[serde(default)]
    pub confirm_terminate: Option<bool>,
}

impl CommandRequest for RuntimeDisableRequest {
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

/// `session_export`（M5-05/ADR-018 附录 A-1；ADR-021 §2.1-8）：单会话 JSONL 导出。
///
/// `target_dir` 缺省 = 应用 `archives/` 目录；提供时经系统选择器 canonicalize
/// （存在目录）后进入后端执行空间护栏（M3-04 同口径）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionExportRequest {
    pub session_id: String,
    #[serde(default)]
    pub target_dir: Option<String>,
}

impl SessionExportRequest {
    /// 外部目标目录的规范化结果（缺省来源为 `None`）。
    pub fn canonical_target_dir(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.target_dir {
            Some(target_dir) => path::validate_backup_target_dir(target_dir).map(Some),
            None => Ok(None),
        }
    }
}

impl CommandRequest for SessionExportRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        if let Some(target_dir) = &self.target_dir {
            ensure_not_empty(target_dir, "target_dir")?;
        }
        Ok(())
    }
}

/// `archive_export`（M5-05/ADR-018 附录 A-1）：全库归档导出（容量「一键归档」）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ArchiveExportRequest {
    #[serde(default)]
    pub target_dir: Option<String>,
}

impl ArchiveExportRequest {
    /// 外部目标目录的规范化结果（缺省来源为 `None`）。
    pub fn canonical_target_dir(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.target_dir {
            Some(target_dir) => path::validate_backup_target_dir(target_dir).map(Some),
            None => Ok(None),
        }
    }
}

impl CommandRequest for ArchiveExportRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(target_dir) = &self.target_dir {
            ensure_not_empty(target_dir, "target_dir")?;
        }
        Ok(())
    }
}

/// 审计 actor 白名单（ADR-018 附录 A-1：`user` / `agent` / `system`）。
pub const AUDIT_ACTORS: [&str; 3] = ["user", "agent", "system"];
/// 审计 action 上限（ADR-018 附录 A-1：`action` ≤64 字符）。
pub const MAX_AUDIT_ACTION_CHARS: usize = 64;
/// 审计 resource 上限（等值过滤目标；与动作字段同量级）。
pub const MAX_AUDIT_RESOURCE_CHARS: usize = 256;

/// 审计过滤字段校验（`audit_query` / `audit_export` 共用；ADR-018 附录 A-1）。
fn validate_audit_filters(
    session_id: Option<&str>,
    actor: Option<&str>,
    action: Option<&str>,
    resource: Option<&str>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
) -> Result<(), IpcError> {
    if let Some(session_id) = session_id {
        if !is_ulid(session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
    }
    if let Some(actor) = actor {
        if !AUDIT_ACTORS.contains(&actor) {
            return Err(IpcError::invalid_enum(
                "actor",
                format!("必须为 {AUDIT_ACTORS:?} 之一"),
            ));
        }
    }
    if let Some(action) = action {
        ensure_not_empty(action, "action")?;
        ensure_max_chars(action, "action", MAX_AUDIT_ACTION_CHARS)?;
        if !action
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(IpcError::invalid_format(
                "action",
                "仅允许小写字母、数字与 . _ -",
            ));
        }
    }
    if let Some(resource) = resource {
        ensure_not_empty(resource, "resource")?;
        ensure_max_chars(resource, "resource", MAX_AUDIT_RESOURCE_CHARS)?;
        reject_control_chars(resource, "resource", false)?;
    }
    if let Some(from_ts) = from_ts {
        if from_ts < 0 {
            return Err(IpcError::out_of_range("from_ts", "必须为非负毫秒时间戳"));
        }
    }
    if let Some(to_ts) = to_ts {
        if to_ts < 0 {
            return Err(IpcError::out_of_range("to_ts", "必须为非负毫秒时间戳"));
        }
    }
    if let (Some(from_ts), Some(to_ts)) = (from_ts, to_ts) {
        if from_ts > to_ts {
            return Err(IpcError::invalid_value("from_ts 不得大于 to_ts"));
        }
    }
    Ok(())
}

/// `audit_query`（M6-03/ADR-023 §2.1-3；ADR-018 附录 A-1）：
/// 审计查询过滤（会话/时间/actor/action/resource）+ 分页（`limit` ≤500 缺省 100、
/// `offset` ≥0；按 `ts DESC, id DESC` 稳定排序）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AuditQueryRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub from_ts: Option<i64>,
    #[serde(default)]
    pub to_ts: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// 响应预算（ADR-018 A-1：64KiB–1.5MiB，缺省 1MiB）。
    #[serde(default)]
    pub max_bytes: Option<u32>,
    /// 不透明游标（与 `offset` 互斥）。
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl AuditQueryRequest {
    /// 转换为存储层过滤条件（`limit` 缺省 100；上限由校验与存储层双重钳制）。
    pub fn to_store_query(&self) -> Result<aether_store::AuditQuery, IpcError> {
        let offset = usize::try_from(self.offset.unwrap_or(0))
            .map_err(|_| IpcError::out_of_range("offset", "超出本机分页寻址范围"))?;
        self.to_store_query_at(offset)
    }

    /// 指定分页偏移（游标续读用；`next_cursor` 的 `pos` 已含偏移）。
    pub fn to_store_query_at(&self, offset: usize) -> Result<aether_store::AuditQuery, IpcError> {
        Ok(aether_store::AuditQuery {
            session_id: self.session_id.clone(),
            actor: self.actor.clone(),
            action: self.action.clone(),
            resource: self.resource.clone(),
            from_ts: self.from_ts,
            to_ts: self.to_ts,
            limit: self.limit.map(|value| value as usize),
            offset,
        })
    }

    /// 过滤条件指纹（游标复用校验；`limit` 变化也会使游标失效）。
    pub fn filter_fingerprint(&self) -> String {
        let limit = self
            .limit
            .unwrap_or(aether_store::AUDIT_QUERY_DEFAULT_LIMIT as u32)
            .to_string();
        cursor::filter_hash(&[
            cursor::STREAM_AUDIT,
            cursor::stable_text(self.session_id.as_deref()),
            cursor::stable_text(self.actor.as_deref()),
            cursor::stable_text(self.action.as_deref()),
            cursor::stable_text(self.resource.as_deref()),
            &self.from_ts.unwrap_or(-1).to_string(),
            &self.to_ts.unwrap_or(-1).to_string(),
            &limit,
        ])
    }
}

impl CommandRequest for AuditQueryRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_audit_filters(
            self.session_id.as_deref(),
            self.actor.as_deref(),
            self.action.as_deref(),
            self.resource.as_deref(),
            self.from_ts,
            self.to_ts,
        )?;
        match self.limit {
            Some(0) => Err(IpcError::out_of_range(
                "limit",
                "必须在 1..=500 之间（ADR-023）",
            )),
            Some(limit) if limit > aether_store::AUDIT_QUERY_MAX_LIMIT as u32 => {
                Err(IpcError::out_of_range(
                    "limit",
                    format!("审计查询上限 {}", aether_store::AUDIT_QUERY_MAX_LIMIT),
                ))
            }
            _ => Ok(()),
        }?;
        cursor::validate_max_bytes(self.max_bytes, "max_bytes")?;
        if let Some(token) = &self.next_cursor {
            if self.offset.is_some() {
                return Err(IpcError::at_field(
                    IpcErrorCode::InvalidValue,
                    "next_cursor",
                    "next_cursor 与 offset 互斥（游标已包含续读位置）",
                ));
            }
            cursor::validate_cursor(token, cursor::STREAM_AUDIT)?;
        }
        Ok(())
    }
}

/// `audit_export`（M6-03/ADR-023 §2.1-4；ADR-018 附录 A-1）：
/// 过滤条件同 `audit_query`；导出 JSONL + manifest（行数/sha256）；外部路径 +
/// 空间检查与 `session_export` 同口径；原子写（中断不留半成品）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AuditExportRequest {
    #[serde(default)]
    pub target_dir: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub from_ts: Option<i64>,
    #[serde(default)]
    pub to_ts: Option<i64>,
}

impl AuditExportRequest {
    /// 外部目标目录的规范化结果（缺省来源为 `None` = 数据目录 `archives/`）。
    pub fn canonical_target_dir(&self) -> Result<Option<std::path::PathBuf>, IpcError> {
        match &self.target_dir {
            Some(target_dir) => path::validate_backup_target_dir(target_dir).map(Some),
            None => Ok(None),
        }
    }

    /// 转换为存储层过滤条件（导出忽略分页；全量命中行）。
    pub fn to_store_query(&self) -> aether_store::AuditQuery {
        aether_store::AuditQuery {
            session_id: self.session_id.clone(),
            actor: self.actor.clone(),
            action: self.action.clone(),
            resource: self.resource.clone(),
            from_ts: self.from_ts,
            to_ts: self.to_ts,
            limit: None,
            offset: 0,
        }
    }
}

impl CommandRequest for AuditExportRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_audit_filters(
            self.session_id.as_deref(),
            self.actor.as_deref(),
            self.action.as_deref(),
            self.resource.as_deref(),
            self.from_ts,
            self.to_ts,
        )?;
        if let Some(target_dir) = &self.target_dir {
            ensure_not_empty(target_dir, "target_dir")?;
        }
        Ok(())
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

/// 预设名称上限（M7-09/ADR-026 §2.2-2：≤64 字符；与核心常量同源）。
pub const MAX_PRESET_NAME_CHARS: usize = aether_core::PRESET_NAME_MAX_CHARS;
/// 预设系统提示词上限（M7-09/ADR-026 §2.2-2：≤32KiB；按 UTF-8 字节数）。
pub const MAX_PRESET_PROMPT_BYTES: usize = aether_core::PRESET_SYSTEM_PROMPT_MAX_BYTES;

/// 预设名称校验（非空 + ≤64 字符 + 无控制字符；ADR-026 §2.2-2）。
fn validate_preset_name(name: &str) -> Result<(), IpcError> {
    ensure_not_empty(name, "name")?;
    ensure_max_chars(name, "name", MAX_PRESET_NAME_CHARS)?;
    reject_control_chars(name, "name", false)
}

/// 预设系统提示词校验（≤32KiB；允许为空——空提示词合成时保留块头，来源仍可追溯）。
fn validate_preset_prompt(system_prompt: &str) -> Result<(), IpcError> {
    ensure_max_bytes(system_prompt, "system_prompt", MAX_PRESET_PROMPT_BYTES)
}

/// `presets_list`（M7-09/ADR-026；ADR-014 附录 C-1 登记为无参数命令）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PresetsListRequest {}

impl CommandRequest for PresetsListRequest {}

/// `preset_add`（M7-09/ADR-026 §2.2-2）：新增自定义预设（`is_builtin=0`）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PresetAddRequest {
    /// 名称（≤64 字符；非空）。
    pub name: String,
    /// 系统提示词（≤32KiB）。
    pub system_prompt: String,
}

impl CommandRequest for PresetAddRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_preset_name(&self.name)?;
        validate_preset_prompt(&self.system_prompt)
    }
}

/// `preset_update`（M7-09/ADR-026 §2.2-2）：整体更新名称/提示词（两者至少一项；
/// 内置预设**可编辑**，仅删除受保护）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PresetUpdateRequest {
    /// 预设 id（ULID；查无 → `preset_not_found`）。
    pub preset_id: String,
    /// 新名称（缺省 = 不变；≤64 字符）。
    #[serde(default)]
    pub name: Option<String>,
    /// 新系统提示词（缺省 = 不变；≤32KiB）。
    #[serde(default)]
    pub system_prompt: Option<String>,
}

impl CommandRequest for PresetUpdateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.preset_id) {
            return Err(IpcError::invalid_format("preset_id", "必须是 26 位 ULID"));
        }
        if self.name.is_none() && self.system_prompt.is_none() {
            return Err(IpcError::invalid_value(
                "必须至少提供 name 或 system_prompt 之一（空请求不产生变更）",
            ));
        }
        if let Some(name) = &self.name {
            validate_preset_name(name)?;
        }
        if let Some(system_prompt) = &self.system_prompt {
            validate_preset_prompt(system_prompt)?;
        }
        Ok(())
    }
}

/// `preset_delete`（M7-09/ADR-026 §2.2-2）：删除预设；内置 → `builtin_preset_undeletable`。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PresetDeleteRequest {
    /// 预设 id（ULID；查无 → `preset_not_found`）。
    pub preset_id: String,
}

impl CommandRequest for PresetDeleteRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.preset_id) {
            return Err(IpcError::invalid_format("preset_id", "必须是 26 位 ULID"));
        }
        Ok(())
    }
}

// ===== M7-08/ADR-026 §2.1：工作区管理与多工作区（五命令；ADR-014 附录 C-1 登记）=====

/// 工作区名称上限（ADR-026 §2.1-1：≤64 字符）。
pub const WORKSPACE_NAME_MAX_CHARS: usize = 64;

/// 工作区名称校验（非空、≤64 字符、无控制字符、首尾无空白——保证存储形态稳定）。
fn validate_workspace_name(name: &str) -> Result<(), IpcError> {
    if name.trim() != name {
        return Err(IpcError::invalid_format("name", "首尾不得包含空白字符"));
    }
    ensure_not_empty(name, "name")?;
    ensure_max_chars(name, "name", WORKSPACE_NAME_MAX_CHARS)?;
    reject_control_chars(name, "name", false)
}

/// `workspaces_list`（M7-08/ADR-026 §2.1-1；无参数命令）。
#[derive(Debug, Default, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspacesListRequest {}

impl CommandRequest for WorkspacesListRequest {}

/// `workspace_create`（M7-08/ADR-026 §2.1-1）：`root_path` canonicalize + 存在目录 +
/// 同步盘拒绝（A4 口径，复用 `workspace_set` 校验链）；同根路径已存在 → `invalid_value`。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCreateRequest {
    /// 名称（≤64 字符；非空）。
    pub name: String,
    /// 工作区根目录（canonicalize 后的绝对路径；同步盘拒绝）。
    pub root_path: String,
}

impl WorkspaceCreateRequest {
    /// 规范化后的工作区根（命令层走查用）。
    pub fn canonical_root_path(&self) -> Result<std::path::PathBuf, IpcError> {
        path::validate_workspace_root(&self.root_path)
    }
}

impl CommandRequest for WorkspaceCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_workspace_name(&self.name)?;
        path::validate_workspace_root(&self.root_path).map(|_| ())
    }
}

/// `workspace_update`（M7-08/ADR-026 §2.1-1）：**仅改名**；`root_path` 不可变
/// （防权限基准漂移），因此不提供 root 字段（未知字段严格拒绝）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceUpdateRequest {
    /// 工作区 id（ULID；查无 → `workspace_not_found`）。
    pub workspace_id: String,
    /// 新名称（≤64 字符；非空）。
    pub name: String,
}

impl CommandRequest for WorkspaceUpdateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.workspace_id) {
            return Err(IpcError::invalid_format(
                "workspace_id",
                "必须是 26 位 ULID",
            ));
        }
        validate_workspace_name(&self.name)
    }
}

/// `workspace_delete`（M7-08/ADR-026 §2.1-1）：存在会话引用 → `workspace_in_use`
/// （不级联、不改派）；只删行，不删磁盘文件。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteRequest {
    /// 工作区 id（ULID；查无 → `workspace_not_found`）。
    pub workspace_id: String,
}

impl CommandRequest for WorkspaceDeleteRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.workspace_id) {
            return Err(IpcError::invalid_format(
                "workspace_id",
                "必须是 26 位 ULID",
            ));
        }
        Ok(())
    }
}

/// `session_workspace_assign`（M7-08/ADR-026 §2.1-1/2）：会话改派工作区。
///
/// 在途 run（`running`/`queued`）或 `waiting_permission` → `session_busy`（稳定业务码
/// 前缀，`invalid_value`，ADR-018 附录 A-7）；改派后记忆注入源切换、**权限基准不重算**
/// （R15/不变量 11）。
#[derive(Debug, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkspaceAssignRequest {
    pub session_id: String,
    /// 目标工作区 id（ULID；查无 → `workspace_not_found`）。
    pub workspace_id: String,
}

impl CommandRequest for SessionWorkspaceAssignRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        if !is_ulid(&self.workspace_id) {
            return Err(IpcError::invalid_format(
                "workspace_id",
                "必须是 26 位 ULID",
            ));
        }
        Ok(())
    }
}
