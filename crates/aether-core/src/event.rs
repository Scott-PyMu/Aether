//! 事件信封 v1 与附录 B 事件类型（设计 D4 / D12 / 附录 B）。
//!
//! - 信封固定字段：`v/id/session_id/run_id/runtime_id/seq/ts/type/payload`，与
//!   `events` 表列一一对应（D4 实现要点，评审修订 #2；增删必须同一次迁移/同版本完成）；
//! - `type` 清单严格来自附录 B：不自造、不遗漏、不重命名（D12：应用层校验，无 CHECK 枚举）；
//! - 反序列化走 serde 严格校验：信封与各 payload 均 `deny_unknown_fields`。

use std::fmt;
use std::str::FromStr;

use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{
    LogLevel, MessageRole, PermissionDecision, PermissionScope, RuntimeStatus, SessionStatus,
};
use crate::error::{parse_variant, EnvelopeError, UnknownValue};
use crate::ids::{
    EventId, MessageId, PermissionRequestId, RunId, RuntimeId, SessionId, ToolCallId, WorkspaceId,
};

/// 事件模型版本（`events.v` / 信封 `v`，D4）。
pub const EVENT_ENVELOPE_VERSION: u32 = 1;

/// 信封固定字段清单（与 `events` 表列双向一一对应，顺序为信封序列化顺序）。
pub const ENVELOPE_FIELDS: [&str; 9] = [
    "v",
    "id",
    "session_id",
    "run_id",
    "runtime_id",
    "seq",
    "ts",
    "type",
    "payload",
];

/// 附录 B 事件类型清单（MVP 集 + 预留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "session.created")]
    SessionCreated,
    #[serde(rename = "session.updated")]
    SessionUpdated,
    #[serde(rename = "session.status_changed")]
    SessionStatusChanged,
    #[serde(rename = "session.closed")]
    SessionClosed,
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "run.cancelled")]
    RunCancelled,
    #[serde(rename = "message.delta")]
    MessageDelta,
    #[serde(rename = "message.completed")]
    MessageCompleted,
    #[serde(rename = "tool.call_started")]
    ToolCallStarted,
    #[serde(rename = "tool.call_completed")]
    ToolCallCompleted,
    #[serde(rename = "tool.call_failed")]
    ToolCallFailed,
    #[serde(rename = "permission.requested")]
    PermissionRequested,
    #[serde(rename = "permission.resolved")]
    PermissionResolved,
    #[serde(rename = "runtime.status_changed")]
    RuntimeStatusChanged,
    #[serde(rename = "usage")]
    Usage,
    #[serde(rename = "log")]
    Log,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "message.reasoning_delta")]
    MessageReasoningDelta,
    #[serde(rename = "subagent.spawned")]
    SubagentSpawned,
    #[serde(rename = "subagent.completed")]
    SubagentCompleted,
    /// 附录 B 预留 `workflow.*` 的占位类型（P2 启用，届时按附录 B 升级定义，不静默扩展）。
    #[serde(rename = "workflow.*")]
    Workflow,
}

impl EventType {
    /// 附录 B「MVP 集」全部 19 种类型。
    pub const MVP: [Self; 19] = [
        Self::SessionCreated,
        Self::SessionUpdated,
        Self::SessionStatusChanged,
        Self::SessionClosed,
        Self::RunStarted,
        Self::RunCompleted,
        Self::RunFailed,
        Self::RunCancelled,
        Self::MessageDelta,
        Self::MessageCompleted,
        Self::ToolCallStarted,
        Self::ToolCallCompleted,
        Self::ToolCallFailed,
        Self::PermissionRequested,
        Self::PermissionResolved,
        Self::RuntimeStatusChanged,
        Self::Usage,
        Self::Log,
        Self::Error,
    ];

    /// 附录 B「预留」类型（reasoning_delta / subagent.* / workflow.*）占位定义。
    pub const RESERVED: [Self; 4] = [
        Self::MessageReasoningDelta,
        Self::SubagentSpawned,
        Self::SubagentCompleted,
        Self::Workflow,
    ];

    /// 全部类型（MVP + 预留）。
    pub const ALL: [Self; 23] = [
        Self::SessionCreated,
        Self::SessionUpdated,
        Self::SessionStatusChanged,
        Self::SessionClosed,
        Self::RunStarted,
        Self::RunCompleted,
        Self::RunFailed,
        Self::RunCancelled,
        Self::MessageDelta,
        Self::MessageCompleted,
        Self::ToolCallStarted,
        Self::ToolCallCompleted,
        Self::ToolCallFailed,
        Self::PermissionRequested,
        Self::PermissionResolved,
        Self::RuntimeStatusChanged,
        Self::Usage,
        Self::Log,
        Self::Error,
        Self::MessageReasoningDelta,
        Self::SubagentSpawned,
        Self::SubagentCompleted,
        Self::Workflow,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::SessionUpdated => "session.updated",
            Self::SessionStatusChanged => "session.status_changed",
            Self::SessionClosed => "session.closed",
            Self::RunStarted => "run.started",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::RunCancelled => "run.cancelled",
            Self::MessageDelta => "message.delta",
            Self::MessageCompleted => "message.completed",
            Self::ToolCallStarted => "tool.call_started",
            Self::ToolCallCompleted => "tool.call_completed",
            Self::ToolCallFailed => "tool.call_failed",
            Self::PermissionRequested => "permission.requested",
            Self::PermissionResolved => "permission.resolved",
            Self::RuntimeStatusChanged => "runtime.status_changed",
            Self::Usage => "usage",
            Self::Log => "log",
            Self::Error => "error",
            Self::MessageReasoningDelta => "message.reasoning_delta",
            Self::SubagentSpawned => "subagent.spawned",
            Self::SubagentCompleted => "subagent.completed",
            Self::Workflow => "workflow.*",
        }
    }

    pub const fn is_mvp(self) -> bool {
        !self.is_reserved()
    }

    pub const fn is_reserved(self) -> bool {
        matches!(
            self,
            Self::MessageReasoningDelta
                | Self::SubagentSpawned
                | Self::SubagentCompleted
                | Self::Workflow
        )
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventType {
    type Err = UnknownValue;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_variant("EventType", &Self::ALL, Self::as_str, value)
    }
}

/// 会话摘要（`session.created` payload）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub id: SessionId,
    pub runtime_id: RuntimeId,
    pub workspace_id: Option<WorkspaceId>,
    pub title: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `session.created` payload（附录 B：摘要）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreatedPayload {
    pub summary: SessionSummary,
}

/// `session.updated` payload（附录 B：变更字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdatedPayload {
    pub session_id: SessionId,
    pub changed_fields: Vec<String>,
}

/// `session.status_changed` payload（附录 B：from/to）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusChangedPayload {
    pub session_id: SessionId,
    pub from: SessionStatus,
    pub to: SessionStatus,
}

/// `session.closed` payload（附录 B：摘要/关闭时点）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionClosedPayload {
    pub session_id: SessionId,
    pub closed_at: i64,
}

/// `run.started` payload（附录 B：runId）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartedPayload {
    pub run_id: RunId,
}

/// `run.completed` payload（附录 B：runId、usage）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCompletedPayload {
    pub run_id: RunId,
    pub usage: Option<crate::domain::TokenUsage>,
}

/// 错误信息（`run.failed` / `tool.call_failed` / `error` payload，附录 B：code、message、recoverable）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

/// `run.failed` payload（附录 B：runId、error）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFailedPayload {
    pub run_id: RunId,
    pub error: ErrorInfo,
}

/// `run.cancelled` payload（附录 B：runId）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelledPayload {
    pub run_id: RunId,
    pub reason: Option<String>,
}

/// `message.delta` payload（附录 B：text；持久化走 16ms/8KB 合并）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageDeltaPayload {
    pub message_id: MessageId,
    pub text: String,
}

/// `message.reasoning_delta` payload（附录 B 预留：text）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageReasoningDeltaPayload {
    pub message_id: MessageId,
    pub text: String,
}

/// 消息摘要（`message.completed` payload）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSummary {
    pub id: MessageId,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub role: MessageRole,
    pub content: String,
    pub created_at: i64,
}

/// `message.completed` payload（附录 B：message 摘要、usage；保存终稿）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageCompletedPayload {
    pub message: MessageSummary,
    pub usage: Option<crate::domain::TokenUsage>,
}

/// `tool.call_started` payload（附录 B：toolName、args（脱敏））。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallStartedPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub args: Value,
}

/// `tool.call_completed` payload（附录 B：toolName、durationMs）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallCompletedPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub duration_ms: u64,
}

/// `tool.call_failed` payload（附录 B：toolName、durationMs、error）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallFailedPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub duration_ms: u64,
    pub error: ErrorInfo,
}

/// `permission.requested` payload（附录 B：requestId、resource、action；D9 回环）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestedPayload {
    pub request_id: PermissionRequestId,
    pub resource: String,
    pub action: String,
    pub target: Option<String>,
}

/// `permission.resolved` payload（附录 B：requestId、decision）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResolvedPayload {
    pub request_id: PermissionRequestId,
    pub decision: PermissionDecision,
    pub scope: Option<PermissionScope>,
}

/// `runtime.status_changed` payload（附录 B：runtimeId、from/to；D5 状态机）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStatusChangedPayload {
    pub runtime_id: RuntimeId,
    pub from: RuntimeStatus,
    pub to: RuntimeStatus,
    pub reason: Option<String>,
}

/// `usage` payload（附录 B：tokens；聚合后写）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsagePayload {
    pub tokens: crate::domain::TokenUsage,
}

/// `log` payload（附录 B：level、message；采样持久化）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogPayload {
    pub level: LogLevel,
    pub message: String,
}

/// `subagent.spawned` payload（附录 B 预留 P2：childSessionId、mode）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSpawnedPayload {
    pub child_session_id: SessionId,
    pub mode: String,
}

/// `subagent.completed` payload（附录 B 预留 P2：childSessionId、verdict）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentCompletedPayload {
    pub child_session_id: SessionId,
    pub verdict: String,
}

/// `workflow.*` payload（附录 B 预留 P2：workflowRunId、nodeId、status）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEventPayload {
    pub workflow_run_id: crate::ids::WorkflowRunId,
    pub node_id: Option<String>,
    pub status: String,
}

/// 事件 payload（与 [`EventType`] 一一对应；序列化为 payload 本体对象）。
#[derive(Debug, Clone, PartialEq)]
pub enum EventPayload {
    SessionCreated(SessionCreatedPayload),
    SessionUpdated(SessionUpdatedPayload),
    SessionStatusChanged(SessionStatusChangedPayload),
    SessionClosed(SessionClosedPayload),
    RunStarted(RunStartedPayload),
    RunCompleted(RunCompletedPayload),
    RunFailed(RunFailedPayload),
    RunCancelled(RunCancelledPayload),
    MessageDelta(MessageDeltaPayload),
    MessageCompleted(MessageCompletedPayload),
    ToolCallStarted(ToolCallStartedPayload),
    ToolCallCompleted(ToolCallCompletedPayload),
    ToolCallFailed(ToolCallFailedPayload),
    PermissionRequested(PermissionRequestedPayload),
    PermissionResolved(PermissionResolvedPayload),
    RuntimeStatusChanged(RuntimeStatusChangedPayload),
    Usage(UsagePayload),
    Log(LogPayload),
    Error(ErrorInfo),
    MessageReasoningDelta(MessageReasoningDeltaPayload),
    SubagentSpawned(SubagentSpawnedPayload),
    SubagentCompleted(SubagentCompletedPayload),
    Workflow(WorkflowEventPayload),
}

impl EventPayload {
    pub const fn event_type(&self) -> EventType {
        match self {
            Self::SessionCreated(_) => EventType::SessionCreated,
            Self::SessionUpdated(_) => EventType::SessionUpdated,
            Self::SessionStatusChanged(_) => EventType::SessionStatusChanged,
            Self::SessionClosed(_) => EventType::SessionClosed,
            Self::RunStarted(_) => EventType::RunStarted,
            Self::RunCompleted(_) => EventType::RunCompleted,
            Self::RunFailed(_) => EventType::RunFailed,
            Self::RunCancelled(_) => EventType::RunCancelled,
            Self::MessageDelta(_) => EventType::MessageDelta,
            Self::MessageCompleted(_) => EventType::MessageCompleted,
            Self::ToolCallStarted(_) => EventType::ToolCallStarted,
            Self::ToolCallCompleted(_) => EventType::ToolCallCompleted,
            Self::ToolCallFailed(_) => EventType::ToolCallFailed,
            Self::PermissionRequested(_) => EventType::PermissionRequested,
            Self::PermissionResolved(_) => EventType::PermissionResolved,
            Self::RuntimeStatusChanged(_) => EventType::RuntimeStatusChanged,
            Self::Usage(_) => EventType::Usage,
            Self::Log(_) => EventType::Log,
            Self::Error(_) => EventType::Error,
            Self::MessageReasoningDelta(_) => EventType::MessageReasoningDelta,
            Self::SubagentSpawned(_) => EventType::SubagentSpawned,
            Self::SubagentCompleted(_) => EventType::SubagentCompleted,
            Self::Workflow(_) => EventType::Workflow,
        }
    }

    /// 严格解析：payload 必须与 `type` 匹配，且 `deny_unknown_fields` 生效。
    pub fn parse(event_type: EventType, value: Value) -> Result<Self, EnvelopeError> {
        let name = event_type.as_str();
        let parsed = match event_type {
            EventType::SessionCreated => {
                Self::SessionCreated(decode::<SessionCreatedPayload>(name, value)?)
            }
            EventType::SessionUpdated => {
                Self::SessionUpdated(decode::<SessionUpdatedPayload>(name, value)?)
            }
            EventType::SessionStatusChanged => {
                Self::SessionStatusChanged(decode::<SessionStatusChangedPayload>(name, value)?)
            }
            EventType::SessionClosed => {
                Self::SessionClosed(decode::<SessionClosedPayload>(name, value)?)
            }
            EventType::RunStarted => Self::RunStarted(decode::<RunStartedPayload>(name, value)?),
            EventType::RunCompleted => {
                Self::RunCompleted(decode::<RunCompletedPayload>(name, value)?)
            }
            EventType::RunFailed => Self::RunFailed(decode::<RunFailedPayload>(name, value)?),
            EventType::RunCancelled => {
                Self::RunCancelled(decode::<RunCancelledPayload>(name, value)?)
            }
            EventType::MessageDelta => {
                Self::MessageDelta(decode::<MessageDeltaPayload>(name, value)?)
            }
            EventType::MessageCompleted => {
                Self::MessageCompleted(decode::<MessageCompletedPayload>(name, value)?)
            }
            EventType::ToolCallStarted => {
                Self::ToolCallStarted(decode::<ToolCallStartedPayload>(name, value)?)
            }
            EventType::ToolCallCompleted => {
                Self::ToolCallCompleted(decode::<ToolCallCompletedPayload>(name, value)?)
            }
            EventType::ToolCallFailed => {
                Self::ToolCallFailed(decode::<ToolCallFailedPayload>(name, value)?)
            }
            EventType::PermissionRequested => {
                Self::PermissionRequested(decode::<PermissionRequestedPayload>(name, value)?)
            }
            EventType::PermissionResolved => {
                Self::PermissionResolved(decode::<PermissionResolvedPayload>(name, value)?)
            }
            EventType::RuntimeStatusChanged => {
                Self::RuntimeStatusChanged(decode::<RuntimeStatusChangedPayload>(name, value)?)
            }
            EventType::Usage => Self::Usage(decode::<UsagePayload>(name, value)?),
            EventType::Log => Self::Log(decode::<LogPayload>(name, value)?),
            EventType::Error => Self::Error(decode::<ErrorInfo>(name, value)?),
            EventType::MessageReasoningDelta => {
                Self::MessageReasoningDelta(decode::<MessageReasoningDeltaPayload>(name, value)?)
            }
            EventType::SubagentSpawned => {
                Self::SubagentSpawned(decode::<SubagentSpawnedPayload>(name, value)?)
            }
            EventType::SubagentCompleted => {
                Self::SubagentCompleted(decode::<SubagentCompletedPayload>(name, value)?)
            }
            EventType::Workflow => Self::Workflow(decode::<WorkflowEventPayload>(name, value)?),
        };
        Ok(parsed)
    }

    /// payload 本体 JSON（`events.payload` 列内容）。
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

fn decode<T>(event_type: &str, value: Value) -> Result<T, EnvelopeError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value)
        .map_err(|error| EnvelopeError::invalid_payload(event_type, &error))
}

impl Serialize for EventPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::SessionCreated(payload) => payload.serialize(serializer),
            Self::SessionUpdated(payload) => payload.serialize(serializer),
            Self::SessionStatusChanged(payload) => payload.serialize(serializer),
            Self::SessionClosed(payload) => payload.serialize(serializer),
            Self::RunStarted(payload) => payload.serialize(serializer),
            Self::RunCompleted(payload) => payload.serialize(serializer),
            Self::RunFailed(payload) => payload.serialize(serializer),
            Self::RunCancelled(payload) => payload.serialize(serializer),
            Self::MessageDelta(payload) => payload.serialize(serializer),
            Self::MessageCompleted(payload) => payload.serialize(serializer),
            Self::ToolCallStarted(payload) => payload.serialize(serializer),
            Self::ToolCallCompleted(payload) => payload.serialize(serializer),
            Self::ToolCallFailed(payload) => payload.serialize(serializer),
            Self::PermissionRequested(payload) => payload.serialize(serializer),
            Self::PermissionResolved(payload) => payload.serialize(serializer),
            Self::RuntimeStatusChanged(payload) => payload.serialize(serializer),
            Self::Usage(payload) => payload.serialize(serializer),
            Self::Log(payload) => payload.serialize(serializer),
            Self::Error(payload) => payload.serialize(serializer),
            Self::MessageReasoningDelta(payload) => payload.serialize(serializer),
            Self::SubagentSpawned(payload) => payload.serialize(serializer),
            Self::SubagentCompleted(payload) => payload.serialize(serializer),
            Self::Workflow(payload) => payload.serialize(serializer),
        }
    }
}

/// 事件信封 v1（D4 固定 9 字段，与 `events` 表列一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    /// 事件模型版本（当前恒为 [`EVENT_ENVELOPE_VERSION`]）。
    pub v: u32,
    /// 事件唯一 ID（ULID，D4 幂等去重）。
    pub id: EventId,
    /// 会话 ID（`events.session_id`）。
    pub session_id: SessionId,
    /// 运行 ID（可空；`events.run_id`）。
    pub run_id: Option<RunId>,
    /// 运行时 ID（`events.runtime_id`，与 D4 信封一致）。
    pub runtime_id: RuntimeId,
    /// 会话内单调唯一序号（单一 sequencer 分配，D4）。
    pub seq: u64,
    /// 事件时间（Unix epoch 毫秒）。
    pub ts: i64,
    /// 事件 payload（携带 `type` 语义）。
    pub payload: EventPayload,
}

impl EventEnvelope {
    pub const fn event_type(&self) -> EventType {
        self.payload.event_type()
    }

    /// 语义校验（反序列化路径已内联执行；用于手工构造后的自检）。
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.v != EVENT_ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion {
                found: self.v,
                supported: EVENT_ENVELOPE_VERSION,
            });
        }
        Ok(())
    }

    /// 严格反序列化入口（Normalizer/补读使用）。
    pub fn from_json_str(input: &str) -> Result<Self, EnvelopeError> {
        serde_json::from_str(input).map_err(|error| EnvelopeError::MalformedJson {
            reason: error.to_string(),
        })
    }

    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn to_json_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

/// 严格反序列化形状：信封全部字段 + `type` 字符串 + 原始 payload。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    v: u32,
    id: EventId,
    session_id: SessionId,
    run_id: Option<RunId>,
    runtime_id: RuntimeId,
    seq: u64,
    ts: i64,
    #[serde(rename = "type")]
    event_type: String,
    payload: Value,
}

impl RawEnvelope {
    fn into_envelope(self) -> Result<EventEnvelope, EnvelopeError> {
        if self.v != EVENT_ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion {
                found: self.v,
                supported: EVENT_ENVELOPE_VERSION,
            });
        }
        let event_type = match EventType::from_str(&self.event_type) {
            Ok(event_type) => event_type,
            Err(UnknownValue { .. }) => {
                return Err(EnvelopeError::UnknownEventType {
                    event_type: self.event_type,
                })
            }
        };
        let payload = EventPayload::parse(event_type, self.payload)?;
        Ok(EventEnvelope {
            v: self.v,
            id: self.id,
            session_id: self.session_id,
            run_id: self.run_id,
            runtime_id: self.runtime_id,
            seq: self.seq,
            ts: self.ts,
            payload,
        })
    }
}

impl Serialize for EventEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("EventEnvelope", ENVELOPE_FIELDS.len())?;
        state.serialize_field(ENVELOPE_FIELDS[0], &self.v)?;
        state.serialize_field(ENVELOPE_FIELDS[1], &self.id)?;
        state.serialize_field(ENVELOPE_FIELDS[2], &self.session_id)?;
        state.serialize_field(ENVELOPE_FIELDS[3], &self.run_id)?;
        state.serialize_field(ENVELOPE_FIELDS[4], &self.runtime_id)?;
        state.serialize_field(ENVELOPE_FIELDS[5], &self.seq)?;
        state.serialize_field(ENVELOPE_FIELDS[6], &self.ts)?;
        state.serialize_field(ENVELOPE_FIELDS[7], self.payload.event_type().as_str())?;
        state.serialize_field(ENVELOPE_FIELDS[8], &self.payload)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEnvelope::deserialize(deserializer)?;
        raw.into_envelope().map_err(D::Error::custom)
    }
}
