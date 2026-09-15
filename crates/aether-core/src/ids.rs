//! 领域标识符（事件信封与实体共用，设计 D4 / 附录 C）。
//!
//! 类型为不透明字符串 newtype：严格拒绝空串，避免「空 ID 入库」这类静默错误。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::EnvelopeError;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 构造 ID；空串返回 [`EnvelopeError::EmptyId`]。
            pub fn new(value: impl Into<String>) -> Result<Self, EnvelopeError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(EnvelopeError::EmptyId {
                        field: stringify!($name),
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_id!(
    /// 事件唯一 ID（D4：ULID，全局去重）。
    EventId
);
string_id!(
    /// 会话 ID（`sessions.id`）。
    SessionId
);
string_id!(
    /// 运行 ID（`runs.id`）。
    RunId
);
string_id!(
    /// 运行时 ID（`runtimes.id`；信封 `runtime_id` 与 `events.runtime_id` 一致）。
    RuntimeId
);
string_id!(
    /// 工作区 ID（`workspaces.id`）。
    WorkspaceId
);
string_id!(
    /// 消息 ID（`messages.id`）。
    MessageId
);
string_id!(
    /// 工具调用 ID（`tool.call_*` 事件关联键）。
    ToolCallId
);
string_id!(
    /// 权限请求 ID（`permissions.request_id`，D9 回环）。
    PermissionRequestId
);
string_id!(
    /// 工作流运行 ID（`workflow_runs.id`，P2 预留）。
    WorkflowRunId
);
