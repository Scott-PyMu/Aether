//! 领域模型（设计 D2 核心模型层 / D12 数据先行 / 附录 C）。
//!
//! 实体字段与附录 C 表列一一对应（列名 = 字段名），与 `events` 列同规则；
//! 状态枚举与 DDL 的 `CHECK (... IN (...))` 取值一一对应（D5/D8 状态机口径）。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{parse_variant, UnknownValue};
use crate::ids::{MessageId, RunId, RuntimeId, SessionId, WorkspaceId};

/// 事件/实体内嵌的 token 用量。
///
/// `serde(default)`：缺失字段按 0 处理，与 `sessions.token_usage TEXT NOT NULL DEFAULT '{}'` 语义一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// 会话状态（`sessions.status` CHECK 约束，D2/D8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

impl SessionStatus {
    pub const ALL: [Self; 8] = [
        Self::Creating,
        Self::Idle,
        Self::Running,
        Self::Paused,
        Self::WaitingPermission,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::WaitingPermission => "waiting_permission",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 运行状态（`runs.status` CHECK 约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Timeout,
}

impl RunStatus {
    pub const ALL: [Self; 6] = [
        Self::Queued,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
        Self::Timeout,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

/// 运行时监督状态（`runtimes.status` CHECK 约束，D5：与状态机一一对应，不做映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Cold,
    Starting,
    Ready,
    Degraded,
    Disabled,
}

impl RuntimeStatus {
    pub const ALL: [Self; 5] = [
        Self::Cold,
        Self::Starting,
        Self::Ready,
        Self::Degraded,
        Self::Disabled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
        }
    }
}

/// 消息角色（`messages.role` CHECK 约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl MessageRole {
    pub const ALL: [Self; 4] = [Self::User, Self::Assistant, Self::System, Self::Tool];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}

/// 权限决议（`permissions.decision` CHECK 约束，D9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

impl PermissionDecision {
    pub const ALL: [Self; 3] = [Self::Allow, Self::Deny, Self::Ask];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// 权限作用域（`permissions.scope` CHECK 约束，D9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    Once,
    Session,
    Always,
}

impl PermissionScope {
    pub const ALL: [Self; 3] = [Self::Once, Self::Session, Self::Always];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "session",
            Self::Always => "always",
        }
    }
}

/// 权限请求状态（`permissions.status` CHECK 约束，D9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    Pending,
    Resolved,
    Timeout,
}

impl PermissionStatus {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Resolved, Self::Timeout];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
            Self::Timeout => "timeout",
        }
    }
}

/// 日志级别（`log` 事件 payload，附录 B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

macro_rules! impl_enum_traits {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownValue;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_variant(stringify!($name), &Self::ALL, Self::as_str, value)
            }
        }
    };
}

impl_enum_traits!(SessionStatus);
impl_enum_traits!(RunStatus);
impl_enum_traits!(RuntimeStatus);
impl_enum_traits!(MessageRole);
impl_enum_traits!(PermissionDecision);
impl_enum_traits!(PermissionScope);
impl_enum_traits!(PermissionStatus);
impl_enum_traits!(LogLevel);

/// 运行时注册实体（`runtimes` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Runtime {
    pub id: RuntimeId,
    pub name: String,
    pub kind: String,
    pub version: String,
    pub protocol: String,
    pub capabilities: Vec<String>,
    pub endpoint: Option<String>,
    pub config: serde_json::Value,
    pub status: RuntimeStatus,
    pub status_reason: Option<String>,
    pub last_seen_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 工作区实体（`workspaces` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root_path: String,
    pub memory_files: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话实体（`sessions` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub runtime_id: RuntimeId,
    pub workspace_id: Option<WorkspaceId>,
    pub parent_session_id: Option<SessionId>,
    pub title: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub config: serde_json::Value,
    pub token_usage: TokenUsage,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
}

/// 消息实体（`messages` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub role: MessageRole,
    pub content: String,
    pub content_parts: Option<serde_json::Value>,
    pub tool_calls: Option<serde_json::Value>,
    pub parent_message_id: Option<MessageId>,
    pub seq: u64,
    pub created_at: i64,
}

/// 运行实体（`runs` 表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub session_id: SessionId,
    pub status: RunStatus,
    pub input_message_id: Option<MessageId>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
