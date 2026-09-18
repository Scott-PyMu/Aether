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
    /// 幂等键（ADR-005）：必填 ULID；重复发送同一 `(session_id, client_msg_id)`
    /// 返回既有 message_id/run_id，核心重启后重放同样不重复。
    pub client_msg_id: String,
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

/// `backup_list`（ADR-004）：无参数命令；非空成员一律拒绝。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupListRequest {}

impl CommandRequest for BackupListRequest {}

/// `health`（ADR-007 决策 1）：无参数命令（严格解析：任何成员拒绝）。
///
/// 仅本地 IPC 查询：不落库、不产生事件；返回 `HealthReport`
/// （`storage_state` / `write_queue_depth` / `runtimes` 摘要 / `ts`）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthRequest {}

impl CommandRequest for HealthRequest {}

/// 备份来源（ADR-004 `backup_restore`）：内部备份 id 枚举 或 外部 `.db` 路径。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BackupSource {
    /// 内部备份 id（`backups` 表白名单，仅 ULID 格式层校验；存在性由后端判定）。
    Internal { id: String },
    /// 外部 `.db` 路径（canonicalize + 存在性 + 后缀，D13 七步第一步）。
    External { path: String },
}

#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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

/// `run_retry`（ADR-004）：仅终态 run 可重试；格式层校验 ULID，终态由后端判定。
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGetRequest {}

impl CommandRequest for StartupGetRequest {}

/// `startup_pick_target`（ADR-006）：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupPickTargetRequest {}

impl CommandRequest for StartupPickTargetRequest {}

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
