//! 服务端发起请求（server-initiated request；ADR-029 §4.5；M9-06 DoD-4）。
//!
//! **M9 仅协议预留，不接产品路径**：
//! - `dialog.pick` 只在协议层定义形状与超时/取消/迟到响应语义；
//! - `ref_pick` / `startup_pick_target` 固定为 Electron 壳本地命令（ADR-029 §4.6），
//!   **不**走本模块，59/4 路由计数不变；
//! - 为 P3 MCP elicitation（`mcp.elicit`）与未来的 CLI 选择器预留。
//!
//! 线上形状（ADR-029 §4.5）：
//!
//! ```json
//! // daemon → client
//! {"jsonrpc":"2.0","id":"srv-1","method":"dialog.pick","params":{"kind":"file","title":"选择文件"}}
//! // client → daemon（正常）
//! {"jsonrpc":"2.0","id":"srv-1","result":{"path":"C:\\Users\\me\\notes.md"}}
//! // client → daemon（用户取消；内部原因 cancelled，不是 aether_code）
//! {"jsonrpc":"2.0","id":"srv-1","result":{"path":null}}
//! ```
//!
//! 约束：
//! - 默认超时 300s；对话框 120s（ADR-029 §4.5）；
//! - 客户端的取消/超时返回**内部原因** `cancelled` / `timeout`，不进入
//!   `aether_code` / `business_code` 命名空间；
//! - 超时/取消之后到达的响应按 id **丢弃**并计数（[`PendingServerRequests::late_dropped`]）；
//! - 本模块只做报文/登记表，不写日志、不判定权限、不依赖任何业务 crate（叶子设施）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use serde_json::{json, Map, Value};

use crate::message::{Id, JSONRPC_VERSION};

/// `dialog.pick` 方法名（ADR-029 §16.4 服务端请求登记表）。
pub const DIALOG_PICK_METHOD: &str = "dialog.pick";
/// 服务端请求默认超时（300s；ADR-029 §4.5）。
pub const SERVER_REQUEST_TIMEOUT_MS: u64 = 300_000;
/// 对话框请求超时（120s；ADR-029 §4.5）。
pub const DIALOG_PICK_TIMEOUT_MS: u64 = 120_000;
/// 对话框标题上限（与 D7 的标题上限同口径：256 字符）。
pub const DIALOG_PICK_TITLE_MAX_CHARS: usize = 256;
/// 选择结果路径上限（与 `aether-app` 的路径上限同口径：4096 字符）。
pub const DIALOG_PICK_PATH_MAX_CHARS: usize = 4_096;
/// 已退休 id 的保留窗口（用于识别「迟到响应」；有界，避免长连接无界增长）。
pub const RETIRED_ID_CAPACITY: usize = 256;

/// 选择器类型（`{kind:"file"|"directory"}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    File,
    Directory,
}

impl PickKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }

    /// 严格解析（仅 `file` / `directory`）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "file" => Some(Self::File),
            "directory" => Some(Self::Directory),
            _ => None,
        }
    }
}

/// 请求/响应形状错误（服务端请求的 `params`、client 响应）。
///
/// `reason` 是机器可读原因（`invalid_params` / `invalid_result` / ...），
/// **不**是 `aether_code`；本模块不参与业务错误命名空间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRequestError {
    reason: String,
}

impl ServerRequestError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ServerRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for ServerRequestError {}

/// `dialog.pick` 请求参数（严格：未知成员拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogPickParams {
    pub kind: PickKind,
    pub title: Option<String>,
}

impl DialogPickParams {
    pub fn new(kind: PickKind) -> Self {
        Self { kind, title: None }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// 严格解析（`kind` 必填；`title` 可选/`null` 等价省略；未知成员拒绝）。
    pub fn from_value(value: &Value) -> Result<Self, ServerRequestError> {
        let Value::Object(object) = value else {
            return Err(ServerRequestError::new(
                "dialog.pick 的 params 必须是 JSON 对象",
            ));
        };
        if let Some(unknown) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "kind" | "title"))
        {
            return Err(ServerRequestError::new(format!(
                "dialog.pick 出现未知字段 {unknown:?}"
            )));
        }
        let kind = match object.get("kind") {
            Some(Value::String(text)) => PickKind::parse(text).ok_or_else(|| {
                ServerRequestError::new(format!(
                    "dialog.pick.kind 只允许 file/directory，收到 {text:?}"
                ))
            })?,
            Some(_) => {
                return Err(ServerRequestError::new(
                    "dialog.pick.kind 必须是字符串（file/directory）",
                ))
            }
            None => return Err(ServerRequestError::new("dialog.pick 缺少必填字段 kind")),
        };
        let title = match object.get("title") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => {
                if text.chars().count() > DIALOG_PICK_TITLE_MAX_CHARS {
                    return Err(ServerRequestError::new(format!(
                        "dialog.pick.title 字符数超过上限 {DIALOG_PICK_TITLE_MAX_CHARS}"
                    )));
                }
                if text.chars().any(char::is_control) {
                    return Err(ServerRequestError::new(
                        "dialog.pick.title 包含不允许的控制字符",
                    ));
                }
                Some(text.clone())
            }
            Some(_) => {
                return Err(ServerRequestError::new(
                    "dialog.pick.title 必须是字符串或 null",
                ))
            }
        };
        Ok(Self { kind, title })
    }

    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("kind".to_owned(), Value::from(self.kind.as_str()));
        if let Some(title) = &self.title {
            object.insert("title".to_owned(), Value::from(title.clone()));
        }
        Value::Object(object)
    }
}

/// `dialog.pick` 响应结果（`{path: string|null}`；严格：未知成员拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogPickResult {
    /// `None` = 用户取消（内部原因 `cancelled`，不是错误码）。
    pub path: Option<String>,
}

impl DialogPickResult {
    pub fn cancelled() -> Self {
        Self { path: None }
    }

    pub fn picked(path: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    pub fn from_value(value: &Value) -> Result<Self, ServerRequestError> {
        let Value::Object(object) = value else {
            return Err(ServerRequestError::new(
                "dialog.pick 的 result 必须是 JSON 对象",
            ));
        };
        if let Some(unknown) = object.keys().find(|key| key.as_str() != "path") {
            return Err(ServerRequestError::new(format!(
                "dialog.pick 的 result 出现未知字段 {unknown:?}"
            )));
        }
        let path = match object.get("path") {
            None => {
                return Err(ServerRequestError::new(
                    "dialog.pick 的 result 缺少必填字段 path",
                ))
            }
            Some(Value::Null) => None,
            Some(Value::String(text)) => {
                if text.is_empty() {
                    return Err(ServerRequestError::new(
                        "dialog.pick 的 path 不得为空字符串（取消请用 null）",
                    ));
                }
                if text.chars().count() > DIALOG_PICK_PATH_MAX_CHARS {
                    return Err(ServerRequestError::new(format!(
                        "dialog.pick 的 path 字符数超过上限 {DIALOG_PICK_PATH_MAX_CHARS}"
                    )));
                }
                if text.chars().any(|character| character.is_control()) {
                    return Err(ServerRequestError::new(
                        "dialog.pick 的 path 包含不允许的控制字符",
                    ));
                }
                Some(text.clone())
            }
            Some(_) => {
                return Err(ServerRequestError::new(
                    "dialog.pick 的 path 必须是字符串或 null",
                ))
            }
        };
        Ok(Self { path })
    }

    pub fn to_value(&self) -> Value {
        json!({ "path": self.path })
    }
}

/// 服务端发起请求报文（daemon → client，带 `id`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRequest {
    pub id: Id,
    pub method: String,
    pub params: Value,
}

impl ServerRequest {
    pub fn new(id: Id, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    /// `dialog.pick` 请求（参数已严格校验）。
    pub fn dialog_pick(id: Id, params: &DialogPickParams) -> Self {
        Self::new(id, DIALOG_PICK_METHOD, params.to_value())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": self.id.to_value(),
            "method": self.method,
            "params": self.params,
        })
    }
}

/// client 响应载荷（`result` 或 `error`，二选一）。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientResponse {
    Result(Value),
    Error {
        code: Option<i64>,
        message: Option<String>,
    },
}

/// 收到一条响应后的处置结果。
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseDisposition {
    /// 在期限内命中登记项：交给调用方处理（`dialog.pick` → [`DialogPickResult`]）。
    Accepted {
        id: Id,
        method: String,
        response: ClientResponse,
    },
    /// 超时/取消之后到达（含已退休 id）：按 ADR-029 §4.5 丢弃并计数。
    Late { id: Id },
    /// 从未登记过：忽略（不升级、不 panic）。
    Unknown { id: Id },
}

/// 超时/取消原因（内部原因；不是 `aether_code`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryReason {
    /// 超过请求期限（对话框默认 120s）。
    Timeout,
    /// 客户端/daemon 主动取消（如会话关闭、连接重建）。
    Cancelled,
}

impl ExpiryReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 一条超时/取消的登记项（调用方据此收口等待者）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRequestExpiry {
    pub id: Id,
    pub method: String,
    pub reason: ExpiryReason,
}

/// 登记错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    /// 同一连接内 id 重复（服务端请求 id 必须唯一）。
    DuplicateId,
    /// 超时必须 >0。
    InvalidTimeout,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId => formatter.write_str("服务端请求 id 重复"),
            Self::InvalidTimeout => formatter.write_str("服务端请求超时必须 >0"),
        }
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEntry {
    method: String,
    deadline_ms: u64,
}

/// 待响应服务端请求登记表（有界退休窗口用于识别迟到响应）。
///
/// 时间由调用方注入（`now_ms`），便于测试 300s/120s 边界而无需真实等待。
#[derive(Debug)]
pub struct PendingServerRequests {
    next_id: u64,
    entries: HashMap<Id, PendingEntry>,
    retired_order: VecDeque<Id>,
    retired: HashSet<Id>,
    retired_capacity: usize,
    late_dropped: u64,
}

impl Default for PendingServerRequests {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingServerRequests {
    pub fn new() -> Self {
        Self::with_retired_capacity(RETIRED_ID_CAPACITY)
    }

    pub fn with_retired_capacity(capacity: usize) -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
            retired_order: VecDeque::new(),
            retired: HashSet::new(),
            retired_capacity: capacity.max(1),
            late_dropped: 0,
        }
    }

    /// 分配服务端请求 id（`srv-<n>`；与 client 请求 id 命名空间区分）。
    pub fn next_id(&mut self) -> Id {
        self.next_id = self.next_id.saturating_add(1);
        Id::String(format!("srv-{}", self.next_id))
    }

    /// 登记一条服务端请求（`deadline = now_ms + timeout_ms`）。
    pub fn register(
        &mut self,
        id: Id,
        method: impl Into<String>,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<(), RegistrationError> {
        if timeout_ms == 0 {
            return Err(RegistrationError::InvalidTimeout);
        }
        if self.entries.contains_key(&id) {
            return Err(RegistrationError::DuplicateId);
        }
        self.entries.insert(
            id,
            PendingEntry {
                method: method.into(),
                deadline_ms: now_ms.saturating_add(timeout_ms),
            },
        );
        Ok(())
    }

    /// 登记 `dialog.pick`（固定 120s 超时）；返回可发送的请求报文。
    pub fn register_dialog_pick(
        &mut self,
        params: &DialogPickParams,
        now_ms: u64,
    ) -> Result<ServerRequest, RegistrationError> {
        let id = self.next_id();
        self.register(
            id.clone(),
            DIALOG_PICK_METHOD,
            now_ms,
            DIALOG_PICK_TIMEOUT_MS,
        )?;
        Ok(ServerRequest::dialog_pick(id, params))
    }

    /// 处理一条 client 响应（严格解析 JSON-RPC 形状）。
    pub fn take_response(
        &mut self,
        value: &Value,
        now_ms: u64,
    ) -> Result<ResponseDisposition, ServerRequestError> {
        let (id, response) = parse_client_response(value)?;
        let expired = self
            .entries
            .get(&id)
            .is_some_and(|entry| now_ms >= entry.deadline_ms);
        if expired {
            self.entries.remove(&id);
            self.retire(id.clone());
            self.late_dropped = self.late_dropped.saturating_add(1);
            return Ok(ResponseDisposition::Late { id });
        }
        if let Some(entry) = self.entries.remove(&id) {
            self.retire(id.clone());
            return Ok(ResponseDisposition::Accepted {
                id,
                method: entry.method,
                response,
            });
        }
        if self.retired.contains(&id) {
            self.late_dropped = self.late_dropped.saturating_add(1);
            return Ok(ResponseDisposition::Late { id });
        }
        Ok(ResponseDisposition::Unknown { id })
    }

    /// 摘除全部到期请求（`deadline <= now_ms`），返回按 id 稳定排序的收口列表。
    pub fn expire(&mut self, now_ms: u64) -> Vec<ServerRequestExpiry> {
        let mut expired: Vec<(Id, PendingEntry)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.deadline_ms <= now_ms)
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect();
        expired.sort_by_key(|(id, _)| id_sort_key(id));
        expired
            .into_iter()
            .map(|(id, entry)| {
                self.entries.remove(&id);
                self.retire(id.clone());
                ServerRequestExpiry {
                    id,
                    method: entry.method,
                    reason: ExpiryReason::Timeout,
                }
            })
            .collect()
    }

    /// 主动取消（命中返回收口项；未命中返回 `None`）。
    pub fn cancel(&mut self, id: &Id) -> Option<ServerRequestExpiry> {
        let entry = self.entries.remove(id)?;
        self.retire(id.clone());
        Some(ServerRequestExpiry {
            id: id.clone(),
            method: entry.method,
            reason: ExpiryReason::Cancelled,
        })
    }

    /// 当前待响应数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 迟到响应丢弃计数（诊断）。
    pub fn late_dropped(&self) -> u64 {
        self.late_dropped
    }

    /// 登记中的请求（诊断/测试；按 id 稳定排序）。
    pub fn pending(&self) -> Vec<(Id, String, u64)> {
        let mut items: Vec<(Id, String, u64)> = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.method.clone(), entry.deadline_ms))
            .collect();
        items.sort_by_key(|(id, _, _)| id_sort_key(id));
        items
    }

    fn retire(&mut self, id: Id) {
        if !self.retired.insert(id.clone()) {
            return;
        }
        self.retired_order.push_back(id);
        while self.retired_order.len() > self.retired_capacity {
            if let Some(evicted) = self.retired_order.pop_front() {
                self.retired.remove(&evicted);
            }
        }
    }
}

fn id_sort_key(id: &Id) -> String {
    match id {
        Id::Number(number) => format!("n:{number:020}"),
        Id::String(text) => format!("s:{text}"),
    }
}

fn parse_client_response(value: &Value) -> Result<(Id, ClientResponse), ServerRequestError> {
    let Value::Object(object) = value else {
        return Err(ServerRequestError::new("client 响应必须是 JSON 对象"));
    };
    match object.get("jsonrpc") {
        Some(Value::String(version)) if version == JSONRPC_VERSION => {}
        _ => {
            return Err(ServerRequestError::new(format!(
                "client 响应的 jsonrpc 必须是 {JSONRPC_VERSION:?}"
            )))
        }
    }
    let id = object
        .get("id")
        .and_then(Id::from_value)
        .ok_or_else(|| ServerRequestError::new("client 响应缺少合法 id（整数或字符串）"))?;
    let result = object.get("result");
    let error = object.get("error");
    let response = match (result, error) {
        (Some(result), None) => ClientResponse::Result(result.clone()),
        (None, Some(Value::Object(error))) => ClientResponse::Error {
            code: error.get("code").and_then(Value::as_i64),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        (None, Some(_)) => {
            return Err(ServerRequestError::new(
                "client 响应的 error 必须是 JSON 对象",
            ))
        }
        (Some(_), Some(_)) => {
            return Err(ServerRequestError::new(
                "client 响应不得同时包含 result 与 error",
            ))
        }
        (None, None) => {
            return Err(ServerRequestError::new(
                "client 响应必须包含 result 或 error",
            ))
        }
    };
    Ok((id, response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_pick_params_are_strict() {
        let params = DialogPickParams::from_value(&json!({"kind": "file"})).expect("合法");
        assert_eq!(params.kind, PickKind::File);
        assert_eq!(params.title, None);
        let params = DialogPickParams::from_value(&json!({"kind": "directory", "title": null}))
            .expect("合法");
        assert_eq!(params.kind, PickKind::Directory);
        assert_eq!(params.title, None);
        let params = DialogPickParams::from_value(&json!({"kind": "file", "title": "选择文件"}))
            .expect("合法");
        assert_eq!(
            params.to_value(),
            json!({"kind": "file", "title": "选择文件"})
        );

        for (value, reason) in [
            (json!(null), "对象"),
            (json!({}), "kind"),
            (json!({"kind": "bogus"}), "file/directory"),
            (json!({"kind": 1}), "字符串"),
            (json!({"kind": "file", "extra": true}), "未知字段"),
            (json!({"kind": "file", "title": 7}), "title"),
            (json!({"kind": "file", "title": "a\nb"}), "控制字符"),
        ] {
            let error = DialogPickParams::from_value(&value).expect_err("必须拒绝");
            assert!(
                error.reason().contains(reason),
                "{value} 的原因应包含 {reason}，实际 {}",
                error.reason()
            );
        }
        let long_title = "x".repeat(DIALOG_PICK_TITLE_MAX_CHARS + 1);
        assert!(
            DialogPickParams::from_value(&json!({"kind": "file", "title": long_title})).is_err()
        );
    }

    #[test]
    fn dialog_pick_result_is_strict() {
        assert_eq!(
            DialogPickResult::from_value(&json!({"path": "C:\\a.md"}))
                .expect("合法")
                .path
                .as_deref(),
            Some("C:\\a.md")
        );
        assert_eq!(
            DialogPickResult::from_value(&json!({"path": null}))
                .expect("取消")
                .path,
            None
        );
        for (value, reason) in [
            (json!({}), "path"),
            (json!({"path": ""}), "空字符串"),
            (json!({"path": 3}), "字符串或 null"),
            (json!({"path": "a", "extra": 1}), "未知字段"),
            (json!({"path": "a\0b"}), "控制字符"),
        ] {
            let error = DialogPickResult::from_value(&value).expect_err("必须拒绝");
            assert!(
                error.reason().contains(reason),
                "{value} 的原因应包含 {reason}，实际 {}",
                error.reason()
            );
        }
    }

    #[test]
    fn normal_response_is_accepted_and_removed() {
        let mut pending = PendingServerRequests::new();
        let request = pending
            .register_dialog_pick(&DialogPickParams::new(PickKind::File), 1_000)
            .expect("登记");
        assert_eq!(request.id, Id::String("srv-1".to_owned()));
        assert_eq!(request.to_value()["method"], json!(DIALOG_PICK_METHOD));
        assert_eq!(pending.len(), 1);

        let disposition = pending
            .take_response(
                &json!({"jsonrpc": "2.0", "id": "srv-1", "result": {"path": "C:\\x"}}),
                1_001,
            )
            .expect("响应形状合法");
        match disposition {
            ResponseDisposition::Accepted {
                method, response, ..
            } => {
                assert_eq!(method, DIALOG_PICK_METHOD);
                let ClientResponse::Result(result) = response else {
                    panic!("必须是 result");
                };
                assert_eq!(
                    DialogPickResult::from_value(&result)
                        .expect("结果形状")
                        .path,
                    Some("C:\\x".to_owned())
                );
            }
            other => panic!("期望 Accepted，实际 {other:?}"),
        }
        assert!(pending.is_empty());
        assert_eq!(pending.late_dropped(), 0);
    }

    #[test]
    fn timeout_expires_and_late_response_is_dropped() {
        let mut pending = PendingServerRequests::new();
        pending
            .register(
                Id::String("srv-9".to_owned()),
                DIALOG_PICK_METHOD,
                0,
                DIALOG_PICK_TIMEOUT_MS,
            )
            .expect("登记");
        assert!(pending.expire(DIALOG_PICK_TIMEOUT_MS - 1).is_empty());
        let expired = pending.expire(DIALOG_PICK_TIMEOUT_MS);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].reason, ExpiryReason::Timeout);
        assert_eq!(expired[0].reason.as_str(), "timeout");

        let disposition = pending
            .take_response(
                &json!({"jsonrpc": "2.0", "id": "srv-9", "result": {"path": null}}),
                DIALOG_PICK_TIMEOUT_MS + 1,
            )
            .expect("形状合法");
        assert!(matches!(disposition, ResponseDisposition::Late { .. }));
        assert_eq!(pending.late_dropped(), 1);
        // 未登记 id：Unknown（不计数、不升级）。
        let disposition = pending
            .take_response(
                &json!({"jsonrpc": "2.0", "id": "srv-404", "result": null}),
                1,
            )
            .expect("形状合法");
        assert!(matches!(disposition, ResponseDisposition::Unknown { .. }));
        assert_eq!(pending.late_dropped(), 1);
    }

    #[test]
    fn cancel_removes_and_late_after_cancel_is_dropped() {
        let mut pending = PendingServerRequests::new();
        let request = pending
            .register_dialog_pick(&DialogPickParams::new(PickKind::Directory), 0)
            .expect("登记");
        let expiry = pending.cancel(&request.id).expect("取消命中");
        assert_eq!(expiry.reason, ExpiryReason::Cancelled);
        assert_eq!(expiry.reason.as_str(), "cancelled");
        assert!(pending.is_empty());
        assert!(pending.cancel(&request.id).is_none(), "重复取消未命中");

        let disposition = pending
            .take_response(
                &json!({"jsonrpc": "2.0", "id": request.id.to_value(), "result": {"path": null}}),
                10,
            )
            .expect("形状合法");
        assert!(matches!(disposition, ResponseDisposition::Late { .. }));
        assert_eq!(pending.late_dropped(), 1);
    }

    #[test]
    fn registration_rejects_duplicates_and_zero_timeout() {
        let mut pending = PendingServerRequests::new();
        pending
            .register(Id::Number(1), "dialog.pick", 0, 10)
            .expect("登记");
        assert_eq!(
            pending.register(Id::Number(1), "dialog.pick", 0, 10),
            Err(RegistrationError::DuplicateId)
        );
        assert_eq!(
            pending.register(Id::Number(2), "dialog.pick", 0, 0),
            Err(RegistrationError::InvalidTimeout)
        );
        assert_eq!(pending.pending().len(), 1);
    }

    #[test]
    fn retired_window_is_bounded() {
        let mut pending = PendingServerRequests::with_retired_capacity(2);
        for id in [1, 2, 3] {
            pending
                .register(Id::Number(id), "dialog.pick", 0, 1)
                .expect("登记");
            pending.expire(1);
        }
        // 窗口只保留最近 2 个退休 id；最旧的 1 被淘汰 → 视为 Unknown。
        let oldest = pending
            .take_response(&json!({"jsonrpc": "2.0", "id": 1, "result": null}), 2)
            .expect("形状合法");
        assert!(matches!(oldest, ResponseDisposition::Unknown { .. }));
        let recent = pending
            .take_response(&json!({"jsonrpc": "2.0", "id": 3, "result": null}), 2)
            .expect("形状合法");
        assert!(matches!(recent, ResponseDisposition::Late { .. }));
        assert_eq!(pending.late_dropped(), 1);
    }

    #[test]
    fn malformed_client_response_is_rejected() {
        let mut pending = PendingServerRequests::new();
        for (value, reason) in [
            (json!([]), "对象"),
            (json!({"id": 1, "result": null}), "jsonrpc"),
            (
                json!({"jsonrpc": "1.0", "id": 1, "result": null}),
                "jsonrpc",
            ),
            (json!({"jsonrpc": "2.0", "result": null}), "id"),
            (
                json!({"jsonrpc": "2.0", "id": 1, "result": null, "error": {}}),
                "同时包含",
            ),
            (json!({"jsonrpc": "2.0", "id": 1}), "必须包含"),
            (json!({"jsonrpc": "2.0", "id": 1, "error": 3}), "JSON 对象"),
        ] {
            let error = pending.take_response(&value, 0).expect_err("必须拒绝");
            assert!(
                error.reason().contains(reason),
                "{value} 的原因应包含 {reason}，实际 {}",
                error.reason()
            );
        }
    }
}
