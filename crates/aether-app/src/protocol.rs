//! M9-08 协议类型（传输无关）。
//!
//! 本模块只定义 **线协议 / typed client 生成所需**的稳定形状，不绑定 Tauri 或
//! Electron：
//! - [`AetherEvent`]：`aether.event` 通知与 `aether:event` 通道的 9 字段信封
//!   （ADR-029 §4.4；与 `aether_core::EventEnvelope` 的 JSON 形状一致）；
//! - [`AetherErrorData`]：JSON-RPC `error.data` 的稳定字段
//!   （`aether_code` / `business_code?` / `details?`；ADR-029 §4.3）。
//!
//! `packages/protocol/src/types.ts` 由本模块 + `ipc::dto` 的 `specta::Type`
//! 导出（生成入口 `crates/aether-app/tests/m9_08_protocol.rs`；禁止手改生成物，
//! AGENTS §2.8）。

use serde::{Deserialize, Serialize};

use crate::json_payload::JsonPayload;

/// 事件信封字段全集（9 字段；与设计附录 B / `EventEnvelope` 对齐）。
///
/// 字段顺序不参与线协议语义（JSON 对象无序），但必须恰好 9 个字段；生成物
/// `types.ts` 与 `verify:m9-08` 都按该集合校验。
pub const AETHER_EVENT_FIELDS: [&str; 9] = [
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

/// `aether.event` / `aether:event` 的 9 字段事件信封。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct AetherEvent {
    /// 事件模型版本（当前恒为 `aether_core::EVENT_ENVELOPE_VERSION`）。
    pub v: u32,
    /// 事件唯一 ID（ULID）。
    pub id: String,
    /// 会话 ID。
    pub session_id: String,
    /// 运行 ID（可空）。
    pub run_id: Option<String>,
    /// 运行时 ID。
    pub runtime_id: String,
    /// 会话内单调唯一序号。
    pub seq: u64,
    /// 事件时间（Unix epoch 毫秒）。
    pub ts: i64,
    /// 事件类型（附录 B；JSON 字段名为 `type`）。
    #[serde(rename = "type")]
    pub event_type: String,
    /// 事件 payload（不透明 JSON；由附录 B 类型定义约束）。
    pub payload: JsonPayload,
}

impl AetherEvent {
    /// 从核心事件信封构造（字段一一对应；payload 为 payload 本体 JSON）。
    pub fn from_envelope(
        envelope: &aether_core::EventEnvelope,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            v: envelope.v,
            id: envelope.id.as_str().to_owned(),
            session_id: envelope.session_id.as_str().to_owned(),
            run_id: envelope.run_id.as_ref().map(|id| id.as_str().to_owned()),
            runtime_id: envelope.runtime_id.as_str().to_owned(),
            seq: envelope.seq,
            ts: envelope.ts,
            event_type: envelope.event_type().as_str().to_owned(),
            payload: JsonPayload(envelope.payload.to_value()?),
        })
    }
}

/// JSON-RPC `error.data` 的稳定字段（ADR-029 §4.3）。
///
/// `aether_code` 只取 `IpcErrorCode` 32 枚举；`business_code` 只取 9 项业务码；
/// `details` 保留既有结构化字段（计数 / 路径 / 原因等）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct AetherErrorData {
    pub aether_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub business_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonPayload>,
}
