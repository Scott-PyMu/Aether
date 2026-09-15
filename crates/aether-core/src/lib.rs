//! Aether 核心模型层（设计 D2）。
//!
//! 硬约束（AGENTS.md §2.1 / §2.2）：
//! - 不依赖任何其他内部 crate，不依赖 Tauri 类型；
//! - 禁止 `unwrap()` / `expect()` / `panic!()`（经 workspace clippy lint 强制；
//!   测试代码在 crate 级显式豁免，见下方 `cfg_attr`）。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod domain;
pub mod error;
pub mod event;
pub mod ids;

pub use domain::{
    LogLevel, Message, MessageRole, PermissionDecision, PermissionScope, PermissionStatus, Run,
    RunStatus, Runtime, RuntimeStatus, Session, SessionStatus, TokenUsage, Workspace,
};
pub use error::{EnvelopeError, UnknownValue};
pub use event::{
    ErrorInfo, EventEnvelope, EventPayload, EventType, LogPayload, MessageCompletedPayload,
    MessageDeltaPayload, MessageReasoningDeltaPayload, MessageSummary, PermissionRequestedPayload,
    PermissionResolvedPayload, RunCancelledPayload, RunCompletedPayload, RunFailedPayload,
    RunStartedPayload, RuntimeStatusChangedPayload, SessionClosedPayload, SessionCreatedPayload,
    SessionStatusChangedPayload, SessionSummary, SessionUpdatedPayload, SubagentCompletedPayload,
    SubagentSpawnedPayload, ToolCallCompletedPayload, ToolCallFailedPayload,
    ToolCallStartedPayload, UsagePayload, WorkflowEventPayload, ENVELOPE_FIELDS,
    EVENT_ENVELOPE_VERSION,
};
pub use ids::{
    EventId, MessageId, PermissionRequestId, RunId, RuntimeId, SessionId, ToolCallId,
    WorkflowRunId, WorkspaceId,
};

/// 核心层版本号——取自单一版本来源（工作区 `Cargo.toml`）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_semver() {
        let v = version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "版本号必须为三段式 semver: {v}");
        assert!(
            parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "版本号各段必须为十进制数字: {v}"
        );
    }

    #[test]
    fn version_matches_crate_manifest() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
