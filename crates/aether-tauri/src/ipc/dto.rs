//! D7 命令面（MVP 全集）的请求 DTO。
//!
//! 全部使用 `#[serde(deny_unknown_fields)]`；可选项显式 `#[serde(default)]`。
//! 语义校验（长度 / 枚举 / 格式）在 `validate` 中完成，保持「解析 → 校验 → 下游」
//! 三段式与错误码稳定。

use serde::Deserialize;

use super::error::IpcError;
use super::validate::{
    self, ensure_max_bytes, ensure_max_chars, ensure_not_empty, ensure_value_size, is_ulid,
    reject_control_chars, CommandRequest, MAX_LABEL_CHARS, MAX_MESSAGE_BYTES, MAX_PAGE_LIMIT,
    MAX_SETTING_VALUE_BYTES, MAX_TITLE_CHARS,
};

/// 权限决议（D9：`once` / `session` 授权；`deny` 拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Once,
    Session,
    Deny,
}

/// 会话状态过滤（与附录 C `sessions.status` CHECK 枚举一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
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
/// M1-08 尚无已批准设置项；后续里程碑（M3-05 等）在实现设置能力时，先在此登记
/// 键名并同步 `settings` 表语义，再放开对应命令。未登记键一律 `invalid_enum`。
pub const SETTINGS_KEY_ALLOWLIST: &[&str] = &[];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    pub runtime_id: String,
    pub title: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
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
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSendRequest {
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub client_msg_id: Option<String>,
}

impl CommandRequest for SessionSendRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if !is_ulid(&self.session_id) {
            return Err(IpcError::invalid_format("session_id", "必须是 26 位 ULID"));
        }
        ensure_max_bytes(&self.text, "text", MAX_MESSAGE_BYTES)?;
        reject_control_chars(&self.text, "text", true)?;
        if let Some(client_msg_id) = &self.client_msg_id {
            validate::validate_ascii_id(client_msg_id, "client_msg_id")?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGetRequest {
    pub key: String,
}

impl CommandRequest for SettingsGetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_settings_key(&self.key)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSetRequest {
    pub key: String,
    pub value: serde_json::Value,
}

impl CommandRequest for SettingsSetRequest {
    fn validate(&self) -> Result<(), IpcError> {
        validate_settings_key(&self.key)?;
        ensure_value_size(&self.value, "value", MAX_SETTING_VALUE_BYTES)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupCreateRequest {
    #[serde(default)]
    pub label: Option<String>,
}

impl CommandRequest for BackupCreateRequest {
    fn validate(&self) -> Result<(), IpcError> {
        if let Some(label) = &self.label {
            ensure_not_empty(label, "label")?;
            ensure_max_chars(label, "label", MAX_LABEL_CHARS)?;
            reject_control_chars(label, "label", false)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDiagnosticsRequest {
    pub target_dir: String,
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
