//! M1-02 契约测试：事件信封 v1（D4/D12/附录 B）与领域模型。
//!
//! - DoD1：信封字段 ↔ `events` 表列双向一一对应 + 序列化往返等价；
//! - DoD2：附录 B「MVP 集」payload 校验齐备 + 预留类型占位定义；
//! - DoD3：非法输入矩阵全部拒绝；
//! - DoD4：迁移 0001 的 `events` 表含 `v`/`runtime_id` 列（DDL 静态断言）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use aether_core::{
    ErrorInfo, EventEnvelope, EventId, EventPayload, EventType, LogLevel, LogPayload, Message,
    MessageCompletedPayload, MessageDeltaPayload, MessageId, MessageReasoningDeltaPayload,
    MessageRole, MessageSummary, PermissionDecision, PermissionRequestId,
    PermissionRequestedPayload, PermissionResolvedPayload, PermissionScope, PermissionStatus, Run,
    RunCancelledPayload, RunCompletedPayload, RunFailedPayload, RunId, RunStartedPayload,
    RunStatus, Runtime, RuntimeId, RuntimeStatus, RuntimeStatusChangedPayload, Session,
    SessionClosedPayload, SessionCreatedPayload, SessionId, SessionStatus,
    SessionStatusChangedPayload, SessionSummary, SessionUpdatedPayload, SubagentCompletedPayload,
    SubagentSpawnedPayload, TokenUsage, ToolCallCompletedPayload, ToolCallFailedPayload,
    ToolCallId, ToolCallStartedPayload, UsagePayload, WorkflowEventPayload, WorkflowRunId,
    Workspace, WorkspaceId, ENVELOPE_FIELDS, EVENT_ENVELOPE_VERSION,
};
use serde_json::{json, Value};

const MIGRATION_SQL: &str = include_str!("../../../migrations/0001_init.sql");

// ===== 附录 C DDL 静态解析（无 DB 依赖，Windows 本地可运行） =====

/// 去掉行注释（`-- ...`），避免注释中的逗号/括号干扰解析。
fn migration_without_comments() -> String {
    MIGRATION_SQL
        .lines()
        .map(|line| match line.find("--") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn create_table_body(table: &str) -> String {
    let sql = migration_without_comments();
    let marker = format!("CREATE TABLE {table} (");
    let start = sql
        .find(&marker)
        .unwrap_or_else(|| panic!("迁移 0001 缺少表 {table}"))
        + marker.len();
    let mut depth = 1usize;
    for (offset, ch) in sql[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return sql[start..start + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("表 {table} 定义未闭合");
}

fn split_top_level(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
}

fn is_table_constraint(first_token: &str) -> bool {
    matches!(
        first_token,
        "CHECK" | "PRIMARY" | "UNIQUE" | "FOREIGN" | "CONSTRAINT"
    )
}

/// 返回 `(列名, 声明类型)`（声明类型可为空串）。
fn column_defs(table: &str) -> Vec<(String, String)> {
    split_top_level(&create_table_body(table))
        .iter()
        .filter_map(|part| {
            let text = part.trim();
            if text.is_empty() {
                return None;
            }
            let mut tokens = text.split_whitespace();
            let name = tokens.next()?.to_string();
            if is_table_constraint(&name) {
                return None;
            }
            let declared_type = tokens.next().unwrap_or_default().to_string();
            Some((name, declared_type))
        })
        .collect()
}

fn table_columns(table: &str) -> Vec<String> {
    column_defs(table)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// 解析 `CHECK (<column> IN ('a','b',...))` 的取值清单。
fn check_values(table: &str, column: &str) -> Vec<String> {
    let body = create_table_body(table);
    let marker = format!("CHECK ({column} IN (");
    let start = body
        .find(&marker)
        .unwrap_or_else(|| panic!("表 {table}.{column} 缺少 IN 约束"))
        + marker.len();
    let end = body[start..]
        .find(')')
        .unwrap_or_else(|| panic!("表 {table}.{column} 约束未闭合"))
        + start;
    body[start..end]
        .split(',')
        .map(|token| token.trim().trim_matches('\'').to_string())
        .collect()
}

fn columns_set(table: &str) -> BTreeSet<String> {
    table_columns(table).into_iter().collect()
}

fn json_object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("序列化结果必须为 JSON 对象")
        .keys()
        .cloned()
        .collect()
}

// ===== 夹具 =====

fn sample_envelope(payload: EventPayload) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new("01J000000000000000000000EV").unwrap(),
        session_id: SessionId::new("sess-1").unwrap(),
        run_id: Some(RunId::new("run-1").unwrap()),
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq: 42,
        ts: 1_760_000_000_000,
        payload,
    }
}

fn message_delta_payload() -> MessageDeltaPayload {
    MessageDeltaPayload {
        message_id: MessageId::new("msg-1").unwrap(),
        text: "hello".to_string(),
    }
}

fn base_envelope_json() -> Value {
    sample_envelope(EventPayload::MessageDelta(message_delta_payload()))
        .to_json_value()
        .unwrap()
}

fn parse_ok(value: Value) -> EventEnvelope {
    serde_json::from_value(value).expect("样本必须通过严格反序列化")
}

fn assert_rejected(label: &str, value: Value) {
    match serde_json::from_value::<EventEnvelope>(value) {
        Ok(envelope) => panic!("[{label}] 应被拒绝，实际通过: {envelope:?}"),
        Err(error) => assert!(!error.to_string().is_empty(), "[{label}] 错误信息不应为空"),
    }
}

fn error_info() -> ErrorInfo {
    ErrorInfo {
        code: "E_TEST".to_string(),
        message: "测试错误".to_string(),
        recoverable: false,
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 10,
        output_tokens: 20,
        total_tokens: 30,
    }
}

fn session_summary() -> SessionSummary {
    SessionSummary {
        id: SessionId::new("sess-1").unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: Some(WorkspaceId::new("ws-1").unwrap()),
        title: "会话".to_string(),
        status: SessionStatus::Idle,
        model: Some("mock-model".to_string()),
        created_at: 1,
        updated_at: 2,
    }
}

fn message_summary() -> MessageSummary {
    MessageSummary {
        id: MessageId::new("msg-1").unwrap(),
        session_id: SessionId::new("sess-1").unwrap(),
        run_id: Some(RunId::new("run-1").unwrap()),
        role: MessageRole::Assistant,
        content: "终稿".to_string(),
        created_at: 3,
    }
}

/// 附录 B 全部 23 种类型（MVP 19 + 预留 4）各一份合法 payload。
fn valid_payload_samples() -> Vec<(EventType, Value)> {
    vec![
        (
            EventType::SessionCreated,
            json!({"summary": session_summary()}),
        ),
        (
            EventType::SessionUpdated,
            json!({"session_id": "sess-1", "changed_fields": ["title", "model"]}),
        ),
        (
            EventType::SessionStatusChanged,
            json!({"session_id": "sess-1", "from": "idle", "to": "running"}),
        ),
        (
            EventType::SessionClosed,
            json!({"session_id": "sess-1", "closed_at": 4}),
        ),
        (EventType::RunStarted, json!({"run_id": "run-1"})),
        (
            EventType::RunCompleted,
            json!({"run_id": "run-1", "usage": usage()}),
        ),
        (
            EventType::RunFailed,
            json!({"run_id": "run-1", "error": error_info()}),
        ),
        (
            EventType::RunCancelled,
            json!({"run_id": "run-1", "reason": "用户取消"}),
        ),
        (
            EventType::MessageDelta,
            json!({"message_id": "msg-1", "text": "hello"}),
        ),
        (
            EventType::MessageCompleted,
            json!({"message": message_summary(), "usage": usage()}),
        ),
        (
            EventType::ToolCallStarted,
            json!({"tool_call_id": "tc-1", "tool_name": "read_file", "args": {"path": "a.txt"}}),
        ),
        (
            EventType::ToolCallCompleted,
            json!({"tool_call_id": "tc-1", "tool_name": "read_file", "duration_ms": 12}),
        ),
        (
            EventType::ToolCallFailed,
            json!({"tool_call_id": "tc-1", "tool_name": "read_file", "duration_ms": 12, "error": error_info()}),
        ),
        (
            EventType::PermissionRequested,
            json!({"request_id": "perm-1", "resource": "fs.read", "action": "read", "target": "a.txt"}),
        ),
        (
            EventType::PermissionResolved,
            json!({"request_id": "perm-1", "decision": "allow", "scope": "session"}),
        ),
        (
            EventType::RuntimeStatusChanged,
            json!({"runtime_id": "mock", "from": "starting", "to": "ready", "reason": null}),
        ),
        (EventType::Usage, json!({"tokens": usage()})),
        (
            EventType::Log,
            json!({"level": "info", "message": "started"}),
        ),
        (
            EventType::Error,
            json!({"code": "E_TEST", "message": "测试错误", "recoverable": true}),
        ),
        (
            EventType::MessageReasoningDelta,
            json!({"message_id": "msg-1", "text": "思考中"}),
        ),
        (
            EventType::SubagentSpawned,
            json!({"child_session_id": "sess-child", "mode": "explore"}),
        ),
        (
            EventType::SubagentCompleted,
            json!({"child_session_id": "sess-child", "verdict": "ok"}),
        ),
        (
            EventType::Workflow,
            json!({"workflow_run_id": "wfrun-1", "node_id": null, "status": "running"}),
        ),
    ]
}

// ===== DoD1：信封字段 ↔ events 表列（双向） =====

#[test]
fn envelope_fields_and_events_columns_are_bidirectionally_equal() {
    let columns = table_columns("events");
    let column_set: BTreeSet<&str> = columns.iter().map(String::as_str).collect();
    let field_set: BTreeSet<&str> = ENVELOPE_FIELDS.iter().copied().collect();

    assert_eq!(
        columns.len(),
        ENVELOPE_FIELDS.len(),
        "列数与信封字段数不一致"
    );
    assert_eq!(column_set, field_set, "events 列集合与信封字段集合不一致");

    for column in &columns {
        assert!(
            ENVELOPE_FIELDS.contains(&column.as_str()),
            "表列 {column} 未出现在信封字段中"
        );
    }
    for field in ENVELOPE_FIELDS {
        assert!(
            columns.iter().any(|column| column == field),
            "信封字段 {field} 未出现在 events 表列中"
        );
    }

    let envelope_json = sample_envelope(EventPayload::MessageDelta(message_delta_payload()))
        .to_json_value()
        .unwrap();
    assert_eq!(
        json_object_keys(&envelope_json),
        column_set.iter().map(|s| s.to_string()).collect(),
        "信封序列化键集合与 events 表列集合不一致"
    );

    let declared: BTreeMap<String, String> = column_defs("events").into_iter().collect();
    let expected = [
        ("id", "TEXT"),
        ("session_id", "TEXT"),
        ("run_id", "TEXT"),
        ("runtime_id", "TEXT"),
        ("seq", "INTEGER"),
        ("type", "TEXT"),
        ("payload", "TEXT"),
        ("ts", "INTEGER"),
        ("v", "INTEGER"),
    ];
    for (name, declared_type) in expected {
        assert_eq!(
            declared.get(name).map(String::as_str),
            Some(declared_type),
            "events.{name} 声明类型与附录 C 不一致"
        );
    }
}

#[test]
fn events_table_contains_v_and_runtime_id_columns() {
    let columns = table_columns("events");
    assert!(columns.contains(&"v".to_string()), "events 缺少 v 列");
    assert!(
        columns.contains(&"runtime_id".to_string()),
        "events 缺少 runtime_id 列"
    );
}

#[test]
fn envelope_serialization_roundtrip_is_lossless() {
    for (event_type, payload) in valid_payload_samples() {
        let parsed = EventPayload::parse(event_type, payload).unwrap();
        let envelope = sample_envelope(parsed);
        let text = envelope.to_json_string().unwrap();
        let back = EventEnvelope::from_json_str(&text).unwrap();
        assert_eq!(back, envelope, "{event_type} 序列化往返不等价");
    }
}

#[test]
fn envelope_optional_fields_accept_null_and_absence() {
    let mut with_null = base_envelope_json();
    with_null["run_id"] = Value::Null;
    assert_eq!(parse_ok(with_null).run_id, None);

    let mut without = base_envelope_json();
    without.as_object_mut().unwrap().remove("run_id");
    assert_eq!(parse_ok(without).run_id, None);
}

// ===== DoD2：附录 B 类型清单与 payload 校验 =====

#[test]
fn appendix_b_type_list_is_exact() {
    let expected_mvp = [
        "session.created",
        "session.updated",
        "session.status_changed",
        "session.closed",
        "run.started",
        "run.completed",
        "run.failed",
        "run.cancelled",
        "message.delta",
        "message.completed",
        "tool.call_started",
        "tool.call_completed",
        "tool.call_failed",
        "permission.requested",
        "permission.resolved",
        "runtime.status_changed",
        "usage",
        "log",
        "error",
    ];
    let expected_reserved = [
        "message.reasoning_delta",
        "subagent.spawned",
        "subagent.completed",
        "workflow.*",
    ];

    assert_eq!(EventType::MVP.len(), 19, "MVP 集类型数应为 19");
    assert_eq!(EventType::RESERVED.len(), 4, "预留类型数应为 4");
    assert_eq!(EventType::ALL.len(), 23, "类型总数应为 23");

    let mvp: BTreeSet<&str> = EventType::MVP.iter().map(|t| t.as_str()).collect();
    let reserved: BTreeSet<&str> = EventType::RESERVED.iter().map(|t| t.as_str()).collect();
    assert_eq!(mvp, expected_mvp.iter().copied().collect());
    assert_eq!(reserved, expected_reserved.iter().copied().collect());
    assert!(mvp.is_disjoint(&reserved), "MVP 与预留集合必须不相交");

    let all: BTreeSet<&str> = EventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(all.len(), 23, "类型字符串必须唯一");
    assert_eq!(
        all,
        mvp.union(&reserved).copied().collect(),
        "ALL 必须等于 MVP ∪ RESERVED"
    );

    for event_type in EventType::ALL {
        assert_eq!(event_type.to_string(), event_type.as_str());
        assert_eq!(
            event_type.as_str().parse::<EventType>().unwrap(),
            event_type
        );
        assert_eq!(
            event_type.is_mvp(),
            !event_type.is_reserved(),
            "is_mvp / is_reserved 必须互斥"
        );
        assert_eq!(mvp.contains(event_type.as_str()), event_type.is_mvp());
    }
}

#[test]
fn all_appendix_b_types_have_valid_payload_and_roundtrip() {
    let samples = valid_payload_samples();
    let sample_types: BTreeSet<&str> = samples.iter().map(|(t, _)| t.as_str()).collect();
    let all_types: BTreeSet<&str> = EventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(sample_types, all_types, "样本必须覆盖附录 B 全部类型");

    for (event_type, payload) in samples {
        let parsed = EventPayload::parse(event_type, payload.clone()).unwrap();
        assert_eq!(parsed.event_type(), event_type, "payload 与 type 不匹配");
        assert_eq!(
            parsed.to_value().unwrap(),
            payload,
            "{event_type} payload 序列化不等价"
        );
    }
}

#[test]
fn reserved_types_have_placeholder_definitions() {
    for reserved in EventType::RESERVED {
        assert!(reserved.is_reserved());
        assert!(!reserved.is_mvp());
        let payload = match reserved {
            EventType::MessageReasoningDelta => {
                EventPayload::MessageReasoningDelta(MessageReasoningDeltaPayload {
                    message_id: MessageId::new("msg-1").unwrap(),
                    text: "思考".to_string(),
                })
            }
            EventType::SubagentSpawned => EventPayload::SubagentSpawned(SubagentSpawnedPayload {
                child_session_id: SessionId::new("sess-child").unwrap(),
                mode: "explore".to_string(),
            }),
            EventType::SubagentCompleted => {
                EventPayload::SubagentCompleted(SubagentCompletedPayload {
                    child_session_id: SessionId::new("sess-child").unwrap(),
                    verdict: "ok".to_string(),
                })
            }
            EventType::Workflow => EventPayload::Workflow(WorkflowEventPayload {
                workflow_run_id: WorkflowRunId::new("wfrun-1").unwrap(),
                node_id: None,
                status: "running".to_string(),
            }),
            other => panic!("{other} 不是预留类型"),
        };
        let envelope = sample_envelope(payload);
        let text = envelope.to_json_string().unwrap();
        assert_eq!(EventEnvelope::from_json_str(&text).unwrap(), envelope);
    }

    let workflow: EventType = "workflow.*".parse().unwrap();
    assert_eq!(workflow, EventType::Workflow);
    assert!(
        "workflow.started".parse::<EventType>().is_err(),
        "P2 前 workflow.* 为占位，不接受具体子类型"
    );
}

#[test]
fn mvp_payloads_reject_invalid_samples() {
    let invalid: Vec<(EventType, Value, &str)> = vec![
        (
            EventType::SessionCreated,
            json!({"summary": {"id": "sess-1", "runtime_id": "mock", "workspace_id": null, "title": "t", "status": "sleeping", "model": null, "created_at": 1, "updated_at": 2}}),
            "非法枚举（status）",
        ),
        (
            EventType::SessionCreated,
            json!({"summary": {"id": "sess-1", "runtime_id": "mock", "title": "t", "status": "idle"}}),
            "缺失必需字段（workspace_id 之外的 created_at/updated_at）",
        ),
        (
            EventType::SessionUpdated,
            json!({"session_id": "sess-1", "changed_fields": "title"}),
            "字段类型错误",
        ),
        (
            EventType::SessionStatusChanged,
            json!({"session_id": "sess-1", "from": "bogus", "to": "running"}),
            "非法枚举（from）",
        ),
        (
            EventType::SessionClosed,
            json!({"session_id": "sess-1", "closed_at": "later"}),
            "字段类型错误",
        ),
        (EventType::RunStarted, json!({}), "缺失必需字段 run_id"),
        (
            EventType::RunCompleted,
            json!({"run_id": "run-1", "usage": 5}),
            "usage 类型错误",
        ),
        (
            EventType::RunFailed,
            json!({"run_id": "run-1", "error": {"code": "E", "message": "m"}}),
            "error 缺失必需字段 recoverable",
        ),
        (
            EventType::RunCancelled,
            json!({"run_id": "run-1", "reason": 7}),
            "字段类型错误",
        ),
        (
            EventType::MessageDelta,
            json!({"message_id": "msg-1", "text": "hi", "index": 0}),
            "未知 payload 字段",
        ),
        (
            EventType::MessageCompleted,
            json!({"message": {"id": "msg-1", "session_id": "sess-1", "run_id": null, "role": "robot", "content": "x", "created_at": 1}, "usage": null}),
            "非法枚举（role）",
        ),
        (
            EventType::ToolCallStarted,
            json!({"tool_call_id": "", "tool_name": "read_file", "args": {}}),
            "空 ID",
        ),
        (
            EventType::ToolCallCompleted,
            json!({"tool_call_id": "tc-1", "tool_name": "read_file", "duration_ms": -1}),
            "负 duration_ms",
        ),
        (
            EventType::ToolCallFailed,
            json!({"tool_call_id": "tc-1", "tool_name": "read_file", "duration_ms": 1, "error": "boom"}),
            "error 类型错误",
        ),
        (
            EventType::PermissionRequested,
            json!({"resource": "fs.read", "action": "read", "target": null}),
            "缺失必需字段 request_id",
        ),
        (
            EventType::PermissionResolved,
            json!({"request_id": "perm-1", "decision": "maybe", "scope": null}),
            "非法枚举（decision）",
        ),
        (
            EventType::RuntimeStatusChanged,
            json!({"runtime_id": "mock", "from": "cold", "to": "paused", "reason": null}),
            "非法枚举（to）",
        ),
        (EventType::Usage, json!({}), "缺失必需字段 tokens"),
        (
            EventType::Log,
            json!({"level": "verbose", "message": "hi"}),
            "非法枚举（level）",
        ),
        (
            EventType::Error,
            json!({"code": "E", "message": "m", "recoverable": "yes"}),
            "字段类型错误",
        ),
    ];

    for (event_type, value, label) in invalid {
        let result = EventPayload::parse(event_type, value);
        assert!(result.is_err(), "[{event_type} / {label}] 应被拒绝");
    }
}

#[test]
fn payload_and_type_mismatch_is_rejected() {
    let mut mismatched = base_envelope_json();
    mismatched["type"] = json!("session.closed");
    assert_rejected("payload 与 type 不匹配", mismatched);

    let mut not_object = base_envelope_json();
    not_object["payload"] = json!(5);
    assert_rejected("payload 非对象", not_object);
}

// ===== DoD3：非法输入矩阵 =====

#[test]
fn invalid_envelope_matrix_is_rejected() {
    let mut unknown_field = base_envelope_json();
    unknown_field["extra"] = json!(1);
    assert_rejected("未知信封字段", unknown_field);

    let mut unknown_payload_field = base_envelope_json();
    unknown_payload_field["payload"]["extra"] = json!(1);
    assert_rejected("未知 payload 字段", unknown_payload_field);

    let mut unknown_type = base_envelope_json();
    unknown_type["type"] = json!("session.created_v2");
    assert_rejected("未知 type（改名/自造）", unknown_type);

    let mut negative_seq = base_envelope_json();
    negative_seq["seq"] = json!(-1);
    assert_rejected("负 seq", negative_seq);

    let mut float_seq = base_envelope_json();
    float_seq["seq"] = json!(1.5);
    assert_rejected("非整数 seq", float_seq);

    let mut string_seq = base_envelope_json();
    string_seq["seq"] = json!("42");
    assert_rejected("字符串 seq", string_seq);

    let mut bad_version = base_envelope_json();
    bad_version["v"] = json!(2);
    assert_rejected("不支持的信封版本 v=2", bad_version);

    let mut negative_version = base_envelope_json();
    negative_version["v"] = json!(-1);
    assert_rejected("非法 v（负数）", negative_version);

    let mut missing_type = base_envelope_json();
    missing_type.as_object_mut().unwrap().remove("type");
    assert_rejected("缺失 type", missing_type);

    let mut missing_payload = base_envelope_json();
    missing_payload.as_object_mut().unwrap().remove("payload");
    assert_rejected("缺失 payload", missing_payload);

    let mut missing_session = base_envelope_json();
    missing_session
        .as_object_mut()
        .unwrap()
        .remove("session_id");
    assert_rejected("缺失 session_id", missing_session);

    let mut missing_runtime = base_envelope_json();
    missing_runtime
        .as_object_mut()
        .unwrap()
        .remove("runtime_id");
    assert_rejected("缺失 runtime_id", missing_runtime);

    let mut bad_run_id = base_envelope_json();
    bad_run_id["run_id"] = json!(7);
    assert_rejected("run_id 类型错误", bad_run_id);

    let mut empty_event_id = base_envelope_json();
    empty_event_id["id"] = json!("");
    assert_rejected("空事件 ID", empty_event_id);

    let mut empty_session_id = base_envelope_json();
    empty_session_id["session_id"] = json!("");
    assert_rejected("空 session_id", empty_session_id);

    let mut bad_session_status = base_envelope_json();
    bad_session_status["type"] = json!("session.status_changed");
    bad_session_status["payload"] =
        json!({"session_id": "sess-1", "from": "idle", "to": "sleeping"});
    assert_rejected("非法枚举（session.status_changed.to）", bad_session_status);

    let mut bad_runtime_status = base_envelope_json();
    bad_runtime_status["type"] = json!("runtime.status_changed");
    bad_runtime_status["payload"] =
        json!({"runtime_id": "mock", "from": "cold", "to": "paused", "reason": null});
    assert_rejected("非法枚举（runtime.status_changed.to）", bad_runtime_status);

    let mut bad_log_level = base_envelope_json();
    bad_log_level["type"] = json!("log");
    bad_log_level["payload"] = json!({"level": "verbose", "message": "hi"});
    assert_rejected("非法枚举（log.level）", bad_log_level);

    let mut bad_decision = base_envelope_json();
    bad_decision["type"] = json!("permission.resolved");
    bad_decision["payload"] = json!({"request_id": "perm-1", "decision": "maybe", "scope": "once"});
    assert_rejected("非法枚举（permission.resolved.decision）", bad_decision);

    let mut bad_scope = base_envelope_json();
    bad_scope["type"] = json!("permission.resolved");
    bad_scope["payload"] = json!({"request_id": "perm-1", "decision": "allow", "scope": "forever"});
    assert_rejected("非法枚举（permission.resolved.scope）", bad_scope);
}

#[test]
fn malformed_json_and_manual_validation_errors_are_reported() {
    let malformed = EventEnvelope::from_json_str("{ not json }").unwrap_err();
    assert!(matches!(
        malformed,
        aether_core::EnvelopeError::MalformedJson { .. }
    ));

    let mut envelope = sample_envelope(EventPayload::Log(LogPayload {
        level: LogLevel::Info,
        message: "hi".to_string(),
    }));
    assert!(envelope.validate().is_ok());
    envelope.v = 2;
    let error = envelope.validate().unwrap_err();
    assert!(matches!(
        error,
        aether_core::EnvelopeError::UnsupportedVersion {
            found: 2,
            supported: 1
        }
    ));
}

// ===== 领域模型：状态枚举 ↔ DDL CHECK 约束 =====

fn assert_enum_matches_check(enum_values: &[&str], table: &str, column: &str) {
    let check = check_values(table, column);
    let expected: BTreeSet<&str> = check.iter().map(String::as_str).collect();
    let actual: BTreeSet<&str> = enum_values.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "{table}.{column} 的 CHECK 取值与枚举不一致"
    );
    assert_eq!(
        actual.len(),
        enum_values.len(),
        "{table}.{column} 枚举取值必须唯一"
    );
}

#[test]
fn status_enums_match_ddl_check_constraints() {
    assert_enum_matches_check(
        &SessionStatus::ALL.map(SessionStatus::as_str),
        "sessions",
        "status",
    );
    assert_enum_matches_check(&RunStatus::ALL.map(RunStatus::as_str), "runs", "status");
    assert_enum_matches_check(
        &RuntimeStatus::ALL.map(RuntimeStatus::as_str),
        "runtimes",
        "status",
    );
    assert_enum_matches_check(
        &MessageRole::ALL.map(MessageRole::as_str),
        "messages",
        "role",
    );
    assert_enum_matches_check(
        &PermissionDecision::ALL.map(PermissionDecision::as_str),
        "permissions",
        "decision",
    );
    assert_enum_matches_check(
        &PermissionScope::ALL.map(PermissionScope::as_str),
        "permissions",
        "scope",
    );
    assert_enum_matches_check(
        &PermissionStatus::ALL.map(PermissionStatus::as_str),
        "permissions",
        "status",
    );
}

#[test]
fn enums_serialize_roundtrip_and_reject_unknown_values() {
    for status in SessionStatus::ALL {
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionStatus>(value).unwrap(),
            status
        );
        assert_eq!(status.as_str().parse::<SessionStatus>().unwrap(), status);
    }
    assert!(serde_json::from_value::<SessionStatus>(json!("sleeping")).is_err());
    assert!(serde_json::from_value::<RuntimeStatus>(json!("paused")).is_err());
    assert!(serde_json::from_value::<LogLevel>(json!("verbose")).is_err());
    assert!(serde_json::from_value::<PermissionDecision>(json!("maybe")).is_err());
    assert!(serde_json::from_value::<PermissionScope>(json!("forever")).is_err());
    assert!(serde_json::from_value::<MessageRole>(json!("robot")).is_err());
    assert!(serde_json::from_value::<RunStatus>(json!("done")).is_err());
    assert!(serde_json::from_value::<PermissionStatus>(json!("expired")).is_err());

    for level in LogLevel::ALL {
        assert_eq!(level.as_str().parse::<LogLevel>().unwrap(), level);
    }
}

#[test]
fn display_impls_are_stable() {
    for status in SessionStatus::ALL {
        assert_eq!(status.to_string(), status.as_str());
    }
    for status in RunStatus::ALL {
        assert_eq!(status.to_string(), status.as_str());
    }
    for status in RuntimeStatus::ALL {
        assert_eq!(status.to_string(), status.as_str());
    }
    for role in MessageRole::ALL {
        assert_eq!(role.to_string(), role.as_str());
    }
    for decision in PermissionDecision::ALL {
        assert_eq!(decision.to_string(), decision.as_str());
    }
    for scope in PermissionScope::ALL {
        assert_eq!(scope.to_string(), scope.as_str());
    }
    for status in PermissionStatus::ALL {
        assert_eq!(status.to_string(), status.as_str());
    }
    for level in LogLevel::ALL {
        assert_eq!(level.to_string(), level.as_str());
    }

    let id = SessionId::new("sess-1").unwrap();
    assert_eq!(id.to_string(), "sess-1");
    assert_eq!(id.as_ref(), "sess-1");

    let unknown = "sleeping".parse::<SessionStatus>().unwrap_err();
    assert_eq!(unknown.to_string(), "SessionStatus 取值非法: sleeping");

    let malformed = EventEnvelope::from_json_str("{").unwrap_err();
    assert!(malformed.to_string().contains("信封 JSON 非法"));

    let unsupported = EventEnvelope {
        v: 9,
        ..sample_envelope(EventPayload::MessageDelta(message_delta_payload()))
    }
    .validate()
    .unwrap_err();
    assert!(unsupported.to_string().contains("v=9"));

    let empty = SessionId::new("").unwrap_err();
    assert!(empty.to_string().contains("SessionId"));
}

// ===== 领域模型：实体字段 ↔ 表列 =====

fn sample_runtime() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock 适配器".to_string(),
        kind: "mock".to_string(),
        version: "0.1.0".to_string(),
        protocol: "1.0".to_string(),
        capabilities: vec!["stream".to_string()],
        endpoint: None,
        config: json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: Some(10),
        created_at: 1,
        updated_at: 2,
    }
}

fn sample_workspace() -> Workspace {
    Workspace {
        id: WorkspaceId::new("ws-1").unwrap(),
        name: "工作区".to_string(),
        root_path: "E:\\work".to_string(),
        memory_files: vec!["AGENTS.md".to_string()],
        created_at: 1,
        updated_at: 2,
    }
}

fn sample_session() -> Session {
    Session {
        id: SessionId::new("sess-1").unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: Some(WorkspaceId::new("ws-1").unwrap()),
        parent_session_id: None,
        title: "会话".to_string(),
        status: SessionStatus::Running,
        model: None,
        system_prompt: None,
        config: json!({"native_id": "n-1"}),
        token_usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        },
        created_at: 1,
        updated_at: 2,
        closed_at: None,
    }
}

fn sample_message() -> Message {
    Message {
        id: MessageId::new("msg-1").unwrap(),
        session_id: SessionId::new("sess-1").unwrap(),
        run_id: Some(RunId::new("run-1").unwrap()),
        role: MessageRole::User,
        content: "hi".to_string(),
        content_parts: None,
        tool_calls: None,
        parent_message_id: None,
        seq: 1,
        created_at: 3,
    }
}

fn sample_run() -> Run {
    Run {
        id: RunId::new("run-1").unwrap(),
        session_id: SessionId::new("sess-1").unwrap(),
        status: RunStatus::Running,
        input_message_id: Some(MessageId::new("msg-1").unwrap()),
        error: None,
        started_at: 1,
        finished_at: None,
    }
}

fn assert_entity_matches_table(label: &str, value: &Value, table: &str) {
    assert_eq!(
        json_object_keys(value),
        columns_set(table),
        "{label} 字段集合与 {table} 表列不一致"
    );
}

#[test]
fn entity_fields_match_table_columns() {
    assert_entity_matches_table(
        "Runtime",
        &serde_json::to_value(sample_runtime()).unwrap(),
        "runtimes",
    );
    assert_entity_matches_table(
        "Workspace",
        &serde_json::to_value(sample_workspace()).unwrap(),
        "workspaces",
    );
    assert_entity_matches_table(
        "Session",
        &serde_json::to_value(sample_session()).unwrap(),
        "sessions",
    );
    assert_entity_matches_table(
        "Message",
        &serde_json::to_value(sample_message()).unwrap(),
        "messages",
    );
    assert_entity_matches_table("Run", &serde_json::to_value(sample_run()).unwrap(), "runs");
}

#[test]
fn typed_payload_constructors_roundtrip_through_json() {
    let samples = vec![
        EventPayload::SessionCreated(SessionCreatedPayload {
            summary: session_summary(),
        }),
        EventPayload::SessionUpdated(SessionUpdatedPayload {
            session_id: SessionId::new("sess-1").unwrap(),
            changed_fields: vec!["title".to_string()],
        }),
        EventPayload::SessionStatusChanged(SessionStatusChangedPayload {
            session_id: SessionId::new("sess-1").unwrap(),
            from: SessionStatus::Idle,
            to: SessionStatus::Running,
        }),
        EventPayload::SessionClosed(SessionClosedPayload {
            session_id: SessionId::new("sess-1").unwrap(),
            closed_at: 4,
        }),
        EventPayload::RunStarted(RunStartedPayload {
            run_id: RunId::new("run-1").unwrap(),
        }),
        EventPayload::RunCompleted(RunCompletedPayload {
            run_id: RunId::new("run-1").unwrap(),
            usage: Some(usage()),
        }),
        EventPayload::RunFailed(RunFailedPayload {
            run_id: RunId::new("run-1").unwrap(),
            error: error_info(),
        }),
        EventPayload::RunCancelled(RunCancelledPayload {
            run_id: RunId::new("run-1").unwrap(),
            reason: None,
        }),
        EventPayload::MessageDelta(message_delta_payload()),
        EventPayload::MessageCompleted(MessageCompletedPayload {
            message: message_summary(),
            usage: None,
        }),
        EventPayload::ToolCallStarted(ToolCallStartedPayload {
            tool_call_id: ToolCallId::new("tc-1").unwrap(),
            tool_name: "read_file".to_string(),
            args: json!({"path": "a.txt"}),
        }),
        EventPayload::ToolCallCompleted(ToolCallCompletedPayload {
            tool_call_id: ToolCallId::new("tc-1").unwrap(),
            tool_name: "read_file".to_string(),
            duration_ms: 12,
        }),
        EventPayload::ToolCallFailed(ToolCallFailedPayload {
            tool_call_id: ToolCallId::new("tc-1").unwrap(),
            tool_name: "read_file".to_string(),
            duration_ms: 12,
            error: error_info(),
        }),
        EventPayload::PermissionRequested(PermissionRequestedPayload {
            request_id: PermissionRequestId::new("perm-1").unwrap(),
            resource: "fs.read".to_string(),
            action: "read".to_string(),
            target: Some("a.txt".to_string()),
        }),
        EventPayload::PermissionResolved(PermissionResolvedPayload {
            request_id: PermissionRequestId::new("perm-1").unwrap(),
            decision: PermissionDecision::Allow,
            scope: Some(PermissionScope::Once),
        }),
        EventPayload::RuntimeStatusChanged(RuntimeStatusChangedPayload {
            runtime_id: RuntimeId::new("mock").unwrap(),
            from: RuntimeStatus::Starting,
            to: RuntimeStatus::Ready,
            reason: None,
        }),
        EventPayload::Usage(UsagePayload { tokens: usage() }),
        EventPayload::Log(LogPayload {
            level: LogLevel::Warn,
            message: "警告".to_string(),
        }),
        EventPayload::Error(error_info()),
    ];
    for payload in samples {
        let event_type = payload.event_type();
        assert!(event_type.is_mvp(), "{event_type} 应为 MVP 类型");
        let value = payload.to_value().unwrap();
        assert_eq!(EventPayload::parse(event_type, value).unwrap(), payload);
    }
}
