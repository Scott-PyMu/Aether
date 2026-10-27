//! JSON-RPC 2.0 报文类型（ADR-029 §4.3/§4.4）。
//!
//! 线上形状（与 ADR 示例逐字段一致）：
//!
//! ```json
//! {"jsonrpc":"2.0","id":42,"method":"session_create","params":{...}}
//! {"jsonrpc":"2.0","id":42,"result":{...}}
//! {"jsonrpc":"2.0","id":42,"error":{"code":-32000,"message":"...","data":{
//!   "aether_code":"invalid_value","business_code":"session_busy","details":{...}}}}
//! {"jsonrpc":"2.0","method":"daemon.status","params":{"phase":"ready","since_ms":1}}
//! ```
//!
//! 约束：
//! - `params` 是**内层 DTO**（无 `{payload}` 包装；ADR-029 P1-5 裁定）；
//!   无参方法省略 / `null` / `{}` 三者等价（由 `aether-app` 的 `parse_no_params` 落地）；
//! - 成功无返回值时 `result` 为 `null`（不是缺字段）；
//! - `error.data.aether_code` 只取 `IpcErrorCode` 32 枚举；`business_code` 只取 9 项；
//! - `message` 只用于展示，不参与逻辑判断。

use serde::Serialize;
use serde_json::{Map, Value};

/// JSON-RPC 版本字面量。
pub const JSONRPC_VERSION: &str = "2.0";

/// 请求 id（数字或字符串；`null`/数组/对象/浮点一律视为非法请求）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

impl Id {
    /// 从 JSON 值解析（仅接受整数或字符串）。
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number.as_i64().map(Self::Number),
            Value::String(text) => Some(Self::String(text.clone())),
            _ => None,
        }
    }

    /// 线上 JSON 值。
    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(number) => Value::from(*number),
            Self::String(text) => Value::from(text.clone()),
        }
    }
}

/// 请求（client → daemon）。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Id,
    pub method: String,
    /// 省略时为 `Value::Null`（无参命令三种形态等价）。
    pub params: Value,
}

impl Request {
    /// 严格解析请求报文（`jsonrpc` 必须为 `"2.0"`；`id` 必填且为数字/字符串）。
    ///
    /// 返回 `Err((id, reason))`：`id` 为可回显的请求 id（无法解析时为 `None`），
    /// `reason` 用于 `invalid_request` 的 `data.reason` 与 `message`。
    pub fn from_value(value: &Value) -> Result<Self, (Option<Id>, String)> {
        let Value::Object(object) = value else {
            return Err((None, "请求必须是 JSON 对象".to_owned()));
        };

        let id = match object.get("id") {
            Some(value) => match Id::from_value(value) {
                Some(id) => Some(id),
                None => {
                    return Err((
                        None,
                        "请求 id 必须是整数或字符串（null/浮点/数组/对象非法）".to_owned(),
                    ))
                }
            },
            None => return Err((None, "请求缺少 id（本协议不使用客户端通知）".to_owned())),
        };
        let id = id.unwrap_or(Id::Number(0));

        match object.get("jsonrpc") {
            Some(Value::String(version)) if version == JSONRPC_VERSION => {}
            _ => {
                return Err((
                    Some(id),
                    format!("jsonrpc 字段必须是 \"{JSONRPC_VERSION}\""),
                ))
            }
        }

        let method = match object.get("method") {
            Some(Value::String(method)) if !method.is_empty() => method.clone(),
            Some(Value::String(_)) => return Err((Some(id), "method 不能为空字符串".to_owned())),
            Some(_) => return Err((Some(id), "method 必须是字符串".to_owned())),
            None => return Err((Some(id), "请求缺少 method".to_owned())),
        };

        let params = object.get("params").cloned().unwrap_or(Value::Null);
        Ok(Self { id, method, params })
    }
}

/// 响应（daemon → client）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    /// 未知/非法 id 时序列化为 `null`（JSON-RPC 要求）。
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// 成功响应（无返回值 → `result: null`）。
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// 失败响应。
    pub fn error(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }

    /// 线上 JSON 值。
    pub fn to_value(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(value) => value,
            Err(error) => {
                // 序列化失败属内部不可达路径：返回最小合法错误帧（不 panic）。
                let fallback = ErrorObject::new(
                    crate::code::INTERNAL_ERROR,
                    ErrorData::new("internal").with_detail("serialize_error", error.to_string()),
                    "响应序列化失败",
                );
                fallback_frame(self.id.clone(), fallback)
            }
        }
    }
}

/// 通知（daemon → client；无 `id`）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: method.into(),
            params,
        }
    }

    pub fn to_value(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(value) => value,
            Err(_) => Value::Null,
        }
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    pub data: ErrorData,
}

impl ErrorObject {
    pub fn new(code: i32, data: ErrorData, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    pub fn to_value(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(value) => value,
            Err(_) => Value::Null,
        }
    }
}

/// 错误数据（`error.data`；R-P0-2 命名空间分离）。
///
/// - `aether_code`：**只取** `IpcErrorCode` 32 枚举（`as_str()` 唯一来源）；
/// - `business_code`：**只取** 9 项业务码（`BusinessCode::as_str()`）；
/// - `phase`/`log_path`：预初始化致命失败首帧必填（ADR-029 §4.2）；
/// - `reason`：传输错误的机器可读原因（如 `protocol_mismatch`/`capability_missing`）；
/// - `details`：既有结构化字段（计数、路径、缺失能力清单等）。
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ErrorData {
    pub aether_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub business_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<super::initialize::DaemonPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ErrorData {
    /// 以 `aether_code`（32 枚举之一）构造。
    pub fn new(aether_code: impl Into<String>) -> Self {
        Self {
            aether_code: aether_code.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_business(mut self, business_code: impl Into<String>) -> Self {
        self.business_code = Some(business_code.into());
        self
    }

    #[must_use]
    pub fn with_phase(mut self, phase: super::initialize::DaemonPhase) -> Self {
        self.phase = Some(phase);
        self
    }

    #[must_use]
    pub fn with_log_path(mut self, log_path: impl Into<String>) -> Self {
        self.log_path = Some(log_path.into());
        self
    }

    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 附加单个 `details` 字段（已有对象则合并）。
    #[must_use]
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        let mut map = match self.details.take() {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        map.insert(key.to_owned(), value.into());
        self.details = Some(Value::Object(map));
        self
    }
}

fn fallback_frame(id: Value, error: ErrorObject) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".to_owned(), Value::from(JSONRPC_VERSION));
    object.insert("id".to_owned(), id);
    object.insert("error".to_owned(), error.to_value());
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_accepts_inner_dto_params() {
        let request = Request::from_value(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session_create",
            "params": {"runtime_id": "claude-code"}
        }))
        .expect("合法请求");
        assert_eq!(request.id, Id::Number(7));
        assert_eq!(request.method, "session_create");
        assert_eq!(request.params["runtime_id"], json!("claude-code"));
    }

    #[test]
    fn request_normalizes_omitted_params_to_null() {
        let request = Request::from_value(&json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "runtimes_list"
        }))
        .expect("合法请求");
        assert_eq!(request.params, Value::Null);
        assert_eq!(request.id, Id::String("req-1".to_owned()));
    }

    #[test]
    fn request_rejects_bad_shapes() {
        for (value, expect_reason) in [
            (json!([1, 2, 3]), "必须是 JSON 对象"),
            (json!({"id": 1, "method": "x"}), "jsonrpc"),
            (json!({"jsonrpc": "1.0", "id": 1, "method": "x"}), "jsonrpc"),
            (json!({"jsonrpc": "2.0", "method": "x"}), "缺少 id"),
            (json!({"jsonrpc": "2.0", "id": 1.5, "method": "x"}), "id"),
            (json!({"jsonrpc": "2.0", "id": null, "method": "x"}), "id"),
            (json!({"jsonrpc": "2.0", "id": 1}), "method"),
            (json!({"jsonrpc": "2.0", "id": 1, "method": 3}), "method"),
            (json!({"jsonrpc": "2.0", "id": 1, "method": ""}), "method"),
        ] {
            let error = Request::from_value(&value).expect_err("必须拒绝");
            assert!(
                error.1.contains(expect_reason),
                "原因应包含 {expect_reason}，实际 {}",
                error.1
            );
        }
    }

    #[test]
    fn error_frame_carries_business_code_and_details() {
        let error = ErrorObject::new(
            crate::code::APPLICATION_ERROR,
            ErrorData::new("invalid_value")
                .with_business("session_busy")
                .with_detail("active_runs", 1),
            "会话在途",
        );
        let value = Response::error(Value::from(42), error).to_value();
        assert_eq!(value["jsonrpc"], json!("2.0"));
        assert_eq!(value["id"], json!(42));
        assert!(value.get("result").is_none(), "失败帧不得带 result");
        assert_eq!(
            value["error"]["data"]["aether_code"],
            json!("invalid_value")
        );
        assert_eq!(
            value["error"]["data"]["business_code"],
            json!("session_busy")
        );
        assert_eq!(value["error"]["data"]["details"]["active_runs"], json!(1));
    }

    #[test]
    fn success_frame_is_null_result_when_no_payload() {
        let value = Response::result(Value::from(1), Value::Null).to_value();
        assert_eq!(value["result"], Value::Null);
        assert!(value.get("error").is_none());
    }
}
