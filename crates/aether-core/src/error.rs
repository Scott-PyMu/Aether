//! 错误类型（设计 D4 信封校验 / D12 应用层类型校验）。

use std::fmt;

/// 信封与 payload 的严格校验错误（`deny_unknown_fields` 之外的语义校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// JSON 语法/结构非法（`from_json_str` 入口）。
    MalformedJson { reason: String },
    /// 信封 `v` 不在本版本支持范围内（D4：payload 只增字段不改语义，`v` 递增）。
    UnsupportedVersion { found: u32, supported: u32 },
    /// `type` 不在附录 B 清单内（D12：应用层校验，不自造类型）。
    UnknownEventType { event_type: String },
    /// payload 与 `type` 不匹配，或 payload 字段非法。
    InvalidPayload { event_type: String, reason: String },
    /// ID 为空串。
    EmptyId { field: &'static str },
}

impl EnvelopeError {
    pub(crate) fn invalid_payload(event_type: impl AsRef<str>, error: &serde_json::Error) -> Self {
        Self::InvalidPayload {
            event_type: event_type.as_ref().to_owned(),
            reason: error.to_string(),
        }
    }
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson { reason } => write!(f, "信封 JSON 非法: {reason}"),
            Self::UnsupportedVersion { found, supported } => {
                write!(
                    f,
                    "不支持的事件模型版本 v={found}（当前支持 v={supported}）"
                )
            }
            Self::UnknownEventType { event_type } => {
                write!(f, "未知事件类型: {event_type}（附录 B 清单之外）")
            }
            Self::InvalidPayload { event_type, reason } => {
                write!(f, "事件 payload 校验失败（type={event_type}）: {reason}")
            }
            Self::EmptyId { field } => write!(f, "ID 字段不允许为空: {field}"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// 封闭枚举取值非法（用于 `FromStr`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownValue {
    pub kind: &'static str,
    pub value: String,
}

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} 取值非法: {}", self.kind, self.value)
    }
}

impl std::error::Error for UnknownValue {}

pub(crate) fn parse_variant<T, F>(
    kind: &'static str,
    all: &[T],
    as_str: F,
    value: &str,
) -> Result<T, UnknownValue>
where
    T: Copy,
    F: Fn(T) -> &'static str,
{
    all.iter()
        .copied()
        .find(|candidate| as_str(*candidate) == value)
        .ok_or_else(|| UnknownValue {
            kind,
            value: value.to_owned(),
        })
}
