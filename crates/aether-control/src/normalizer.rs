//! Normalizer（D4 管线第一步）：适配器事件 → 事件类型映射 → serde 严格校验。
//!
//! 设计口径（D4）：
//! - 适配器事件 → Normalizer（映射为事件类型）→ **serde 严格校验** → 会话 sequencer；
//! - 未知字段策略：信封顶层未知字段 **拒绝**（`deny_unknown_fields`）；JSON-RPC 外层未知
//!   成员在 D6 连接层已忽略，不在本层；
//! - `type` 只能使用附录 B 清单；预留类型 P0 不启用（payload 语义未定义）；
//! - `seq` 由核心单一 sequencer 分配（D4）：适配器上报的 `seq` 一律忽略；
//! - 校验失败降级为死信计数（`log(warn)` 由调用方），**不阻断会话**。
//!
//! 纯逻辑、无 I/O，全部可单测。

use aether_core::{
    EventEnvelope, EventId, EventPayload, EventType, RunId, RuntimeId, SessionId,
    EVENT_ENVELOPE_VERSION,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::NormalizeError;

/// 归一化后、尚未分配 `seq` 的事件（信封其余 8 字段已就绪）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEvent {
    pub id: EventId,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub runtime_id: RuntimeId,
    pub ts: i64,
    pub payload: EventPayload,
}

impl PendingEvent {
    /// 由会话 sequencer 分配 `seq` 后固化为事件信封（9 字段）。
    pub fn with_seq(self, seq: u64) -> EventEnvelope {
        EventEnvelope {
            v: EVENT_ENVELOPE_VERSION,
            id: self.id,
            session_id: self.session_id,
            run_id: self.run_id,
            runtime_id: self.runtime_id,
            seq,
            ts: self.ts,
            payload: self.payload,
        }
    }
}

/// 适配器上报事件的严格形状（信封 9 字段 + 适配器 `seq` 位）。
///
/// - `seq`：适配器侧序号，核心**忽略**（D4 单 sequencer），仅为兼容 SDK 形状保留；
/// - `deny_unknown_fields`：归一化后信封顶层未知字段拒绝（D4 未知字段策略）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterEventDraft {
    v: u32,
    id: EventId,
    session_id: SessionId,
    run_id: Option<RunId>,
    runtime_id: RuntimeId,
    #[allow(dead_code)]
    seq: Option<u64>,
    ts: i64,
    #[serde(rename = "type")]
    event_type: String,
    payload: Value,
}

/// 事件归一化器。
pub struct Normalizer;

impl Normalizer {
    /// 严格归一化：形状校验 → 版本校验 → 类型白名单 → payload 严格解析。
    pub fn normalize(raw: &Value) -> Result<PendingEvent, NormalizeError> {
        let draft: AdapterEventDraft =
            serde_json::from_value(raw.clone()).map_err(|error| NormalizeError::Malformed {
                reason: error.to_string(),
            })?;

        if draft.v != EVENT_ENVELOPE_VERSION {
            return Err(NormalizeError::UnsupportedVersion {
                found: draft.v,
                supported: EVENT_ENVELOPE_VERSION,
            });
        }

        let event_type: EventType =
            draft
                .event_type
                .parse()
                .map_err(|_| NormalizeError::UnknownEventType {
                    event_type: draft.event_type.clone(),
                })?;
        if event_type.is_reserved() {
            return Err(NormalizeError::ReservedEventType {
                event_type: draft.event_type,
            });
        }

        let payload = EventPayload::parse(event_type, draft.payload).map_err(|error| {
            NormalizeError::InvalidPayload {
                event_type: draft.event_type.clone(),
                reason: error.to_string(),
            }
        })?;

        Ok(PendingEvent {
            id: draft.id,
            session_id: draft.session_id,
            run_id: draft.run_id,
            runtime_id: draft.runtime_id,
            ts: draft.ts,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use aether_core::LogLevel;
    use serde_json::json;

    use super::*;

    fn raw(event_type: &str, payload: Value) -> Value {
        json!({
            "v": 1,
            "id": "01J00000000000000000000001",
            "session_id": "01J0000000000000000000000S",
            "run_id": null,
            "runtime_id": "mock",
            "seq": 7,
            "ts": 1_700_000_000_000i64,
            "type": event_type,
            "payload": payload,
        })
    }

    #[test]
    fn normalizes_log_event_and_ignores_adapter_seq() {
        let pending = Normalizer::normalize(&raw(
            "log",
            json!({"level": "warn", "message": "适配器上报"}),
        ))
        .expect("合法事件必须通过");
        assert_eq!(pending.payload.event_type(), EventType::Log);
        match &pending.payload {
            EventPayload::Log(payload) => {
                assert_eq!(payload.level, LogLevel::Warn);
                assert_eq!(payload.message, "适配器上报");
            }
            other => panic!("类型不符: {other:?}"),
        }
        // 适配器 seq=7 被忽略：固化时 seq 由调用方（sequencer）给出。
        let envelope = pending.with_seq(1);
        assert_eq!(envelope.seq, 1);
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let mut value = raw("log", json!({"level": "info", "message": "x"}));
        value["future_field"] = json!(true);
        let error = Normalizer::normalize(&value).expect_err("顶层未知字段必须拒绝");
        assert_eq!(error.code(), "malformed_event");
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut value = raw("log", json!({"level": "info", "message": "x"}));
        value["v"] = json!(2);
        let error = Normalizer::normalize(&value).expect_err("未知信封版本必须拒绝");
        assert_eq!(error.code(), "unsupported_event_version");
    }

    #[test]
    fn unknown_and_reserved_types_are_rejected() {
        let error = Normalizer::normalize(&raw("message.self_made", json!({})))
            .expect_err("自造事件类型必须拒绝");
        assert_eq!(error.code(), "unknown_event_type");

        let error = Normalizer::normalize(&raw(
            "message.reasoning_delta",
            json!({"message_id": "m", "text": "x"}),
        ))
        .expect_err("预留类型 P0 未启用");
        assert_eq!(error.code(), "reserved_event_type");
    }

    #[test]
    fn invalid_payload_is_rejected_with_unknown_fields() {
        let error = Normalizer::normalize(&raw("log", json!({"level": "nope", "message": "x"})))
            .expect_err("非法枚举必须拒绝");
        assert_eq!(error.code(), "invalid_event_payload");

        let error = Normalizer::normalize(&raw(
            "log",
            json!({"level": "info", "message": "x", "extra": 1}),
        ))
        .expect_err("payload 未知字段必须按 type 策略拒绝");
        assert_eq!(error.code(), "invalid_event_payload");
    }

    #[test]
    fn missing_required_envelope_field_is_malformed() {
        let mut value = raw("log", json!({"level": "info", "message": "x"}));
        value.as_object_mut().expect("对象").remove("runtime_id");
        let error = Normalizer::normalize(&value).expect_err("缺字段必须拒绝");
        assert_eq!(error.code(), "malformed_event");
    }

    #[test]
    fn all_mvp_event_types_round_trip() {
        let samples: Vec<(&str, Value)> = vec![
            (
                "session.created",
                json!({"summary": {
                    "id": "01J0000000000000000000000S",
                    "runtime_id": "mock",
                    "workspace_id": null,
                    "title": "会话",
                    "status": "idle",
                    "model": null,
                    "created_at": 1,
                    "updated_at": 1,
                }}),
            ),
            (
                "session.updated",
                json!({"session_id": "01J0000000000000000000000S", "changed_fields": ["title"]}),
            ),
            (
                "session.status_changed",
                json!({"session_id": "01J0000000000000000000000S", "from": "idle", "to": "running"}),
            ),
            (
                "session.closed",
                json!({"session_id": "01J0000000000000000000000S", "closed_at": 1}),
            ),
            (
                "run.started",
                json!({"run_id": "01J0000000000000000000000R"}),
            ),
            (
                "run.completed",
                json!({"run_id": "01J0000000000000000000000R", "usage": null}),
            ),
            (
                "run.failed",
                json!({"run_id": "01J0000000000000000000000R",
                       "error": {"code": "boom", "message": "失败", "recoverable": false}}),
            ),
            (
                "run.cancelled",
                json!({"run_id": "01J0000000000000000000000R", "reason": "interrupt"}),
            ),
            (
                "message.delta",
                json!({"message_id": "01J0000000000000000000000M", "text": "片段"}),
            ),
            (
                "message.completed",
                json!({"message": {
                    "id": "01J0000000000000000000000M",
                    "session_id": "01J0000000000000000000000S",
                    "run_id": null,
                    "role": "assistant",
                    "content": "终稿",
                    "created_at": 1,
                }, "usage": null}),
            ),
            (
                "tool.call_started",
                json!({"tool_call_id": "01J0000000000000000000000T", "tool_name": "fs.read", "args": {}}),
            ),
            (
                "tool.call_completed",
                json!({"tool_call_id": "01J0000000000000000000000T", "tool_name": "fs.read", "duration_ms": 3}),
            ),
            (
                "tool.call_failed",
                json!({"tool_call_id": "01J0000000000000000000000T", "tool_name": "fs.read",
                       "duration_ms": 4, "error": {"code": "io", "message": "x", "recoverable": true}}),
            ),
            (
                "permission.requested",
                json!({"request_id": "01J0000000000000000000000P", "resource": "fs.read",
                       "action": "read", "target": "/tmp/x"}),
            ),
            (
                "permission.resolved",
                json!({"request_id": "01J0000000000000000000000P", "decision": "allow", "scope": "once"}),
            ),
            (
                "runtime.status_changed",
                json!({"runtime_id": "mock", "from": "cold", "to": "ready", "reason": null}),
            ),
            (
                "usage",
                json!({"tokens": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}}),
            ),
            ("log", json!({"level": "info", "message": "x"})),
            (
                "error",
                json!({"code": "x", "message": "y", "recoverable": false}),
            ),
        ];
        assert_eq!(samples.len(), EventType::MVP.len(), "MVP 类型必须全部覆盖");
        for (event_type, payload) in samples {
            let pending = Normalizer::normalize(&raw(event_type, payload))
                .unwrap_or_else(|error| panic!("{event_type} 必须通过: {error}"));
            assert_eq!(pending.payload.event_type().as_str(), event_type);
        }
    }
}
