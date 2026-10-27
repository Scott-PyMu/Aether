//! 连接状态机（ADR-029 §4.2/§4.3/§4.8）：`initialize` 门、重复 id、`shutdown` 与错误映射。
//!
//! 状态迁移：
//!
//! ```text
//! AwaitingInitialize --initialize--> Ready --shutdown--> ShuttingDown
//!         |                            |
//!         +-- 业务请求 → -32002         +-- 业务请求 → -32004（已接受关闭）
//! ```
//!
//! 约束：
//! - `initialize` 必须是连接上的第一个请求；重复 `initialize` → `invalid_request`；
//! - 重复 `request id` → `invalid_request`（有界窗口 [`ID_WINDOW`] 内去重；ADR-029 §4.8）；
//! - 未知方法/壳本地命令 → `method_not_found`；业务分发由 [`Server`] 实现；
//! - 超限帧只失败该请求（[`Connection::oversized`]），不改变连接状态、不重启进程；
//! - 内部 outcome 字符串不得作为 `aether_code`：本模块只接受调用方给出的
//!   32 枚举字符串（`IpcErrorCode::as_str()`）。

use std::collections::{HashSet, VecDeque};

use serde_json::Value;

use crate::code;
use crate::initialize::{DaemonPhase, InitializeParams, InitializeResult};
use crate::message::{ErrorData, ErrorObject, Id, Request, Response};

/// 重复 id 去重窗口（连接内保留的最近请求 id 数量）。
///
/// 窗口有界（避免长时间连接的无界内存增长）；超出窗口的旧 id 可复用。
pub const ID_WINDOW: usize = 4096;

/// 业务/协议失败（由 [`Server`] 返回；本 crate 负责编码为 `error` 对象）。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcFailure {
    transport: i32,
    aether_code: String,
    business_code: Option<String>,
    message: String,
    reason: Option<String>,
    details: Option<Value>,
}

impl RpcFailure {
    /// 指定传输码 + `aether_code`（32 枚举之一）。
    pub fn new(transport: i32, aether_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            transport,
            aether_code: aether_code.into(),
            business_code: None,
            message: message.into(),
            reason: None,
            details: None,
        }
    }

    /// 通用传输错误构造。
    pub fn transport(
        transport: i32,
        aether_code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(transport, aether_code, message)
    }

    /// 参数非法（`-32602`；`aether_code` 取 `unknown_field`/`missing_field`/…）。
    pub fn invalid_params(aether_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, aether_code, message)
    }

    /// 应用/域错误（`-32000`；业务分支读 `data.business_code`）。
    pub fn application(aether_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code::APPLICATION_ERROR, aether_code, message)
    }

    /// 未知方法（`-32601`；`aether_code=not_implemented`，与 `aether-app::dispatch` 一致）。
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            code::METHOD_NOT_FOUND,
            "not_implemented",
            format!("未知方法 {method}（命令面 63 条之外；或壳本地命令误入 daemon）"),
        )
        .with_reason("method_not_found")
    }

    /// 内部错误（`-32603`）。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL_ERROR, "internal", message)
    }

    /// 核心未就绪（`-32002`）。
    pub fn core_not_ready(message: impl Into<String>) -> Self {
        Self::new(code::CORE_NOT_READY, "core_not_ready", message)
    }

    #[must_use]
    pub fn with_business(mut self, business_code: impl Into<String>) -> Self {
        self.business_code = Some(business_code.into());
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

    #[must_use]
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        let mut details = match self.details.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        details.insert(key.to_owned(), value.into());
        self.details = Some(Value::Object(details));
        self
    }

    pub fn transport_code(&self) -> i32 {
        self.transport
    }

    pub fn aether_code(&self) -> &str {
        &self.aether_code
    }

    pub fn business_code(&self) -> Option<&str> {
        self.business_code.as_deref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    /// 编码为 JSON-RPC `error` 对象。
    pub fn into_error_object(self) -> ErrorObject {
        let mut data = ErrorData::new(self.aether_code);
        if let Some(business_code) = self.business_code {
            data = data.with_business(business_code);
        }
        if let Some(reason) = self.reason {
            data = data.with_reason(reason);
        }
        if let Some(details) = self.details {
            data = data.with_details(details);
        }
        ErrorObject::new(self.transport, data, self.message)
    }
}

/// 业务分发接口（`aetherd` 以 `aether-app::dispatch` 实现）。
pub trait Server {
    /// 当前 daemon 阶段（`initialize` 响应 / `daemon.status`）。
    fn phase(&self) -> DaemonPhase;
    /// 解析后的数据目录（`initialize` 响应）。
    fn data_dir(&self) -> String;
    /// daemon 版本（workspace 单一版本源）。
    fn daemon_version(&self) -> String;
    /// 核心库版本。
    fn core_version(&self) -> String;
    /// `initialize` 已被接受（daemon 可在此启动后台预热；**不阻塞响应**）。
    fn initialize_accepted(&mut self) {}
    /// 生成 `shutdown` 回执 id。
    fn shutdown_id(&mut self) -> String;
    /// 业务方法分发（59 条 daemon 命令）。
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcFailure>;
}

/// 处理结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// 回写一帧后继续读取。
    Reply(Value),
    /// 回写一帧（`shutdown` 回执）后进入关闭序列。
    ReplyAndClose(Value),
}

/// 连接阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitingInitialize,
    Ready,
    ShuttingDown,
}

/// 单连接状态机（请求串行处理）。
pub struct Connection {
    stage: Stage,
    ids: IdWindow,
}

impl Default for Connection {
    fn default() -> Self {
        Self::new()
    }
}

impl Connection {
    pub fn new() -> Self {
        Self {
            stage: Stage::AwaitingInitialize,
            ids: IdWindow::new(ID_WINDOW),
        }
    }

    /// 是否已完成 `initialize`。
    pub fn initialized(&self) -> bool {
        self.stage != Stage::AwaitingInitialize
    }

    /// 是否已进入关闭序列。
    pub fn shutting_down(&self) -> bool {
        self.stage == Stage::ShuttingDown
    }

    /// 处理一帧（原始字节，不含分隔符）。
    pub fn handle(&mut self, raw: &[u8], server: &mut impl Server) -> Handled {
        let value: Value = match serde_json::from_slice(raw) {
            Ok(value) => value,
            Err(error) => {
                return Handled::Reply(
                    Response::error(
                        Value::Null,
                        RpcFailure::transport(
                            code::PARSE_ERROR,
                            "invalid_json",
                            format!("JSON 解析失败：{error}"),
                        )
                        .with_reason("parse_error")
                        .into_error_object(),
                    )
                    .to_value(),
                )
            }
        };

        if !value.is_object() {
            return Handled::Reply(Self::invalid_request(
                Value::Null,
                "invalid_json",
                "请求必须是 JSON 对象",
                "not_an_object",
            ));
        }

        let request = match Request::from_value(&value) {
            Ok(request) => request,
            Err((id, reason)) => {
                let id = id.map_or(Value::Null, |id| id.to_value());
                return Handled::Reply(Self::invalid_request(
                    id,
                    "invalid_value",
                    reason,
                    "invalid_request",
                ));
            }
        };

        if !self.ids.remember(&request.id) {
            return Handled::Reply(Self::invalid_request(
                request.id.to_value(),
                "invalid_value",
                format!("重复的 request id：{:?}", request.id),
                "duplicate_request_id",
            ));
        }

        let Request { id, method, params } = request;
        match method.as_str() {
            "initialize" => self.handle_initialize(id, &params, server),
            "shutdown" => {
                if self.stage == Stage::ShuttingDown {
                    return Handled::Reply(
                        Response::error(
                            id.to_value(),
                            RpcFailure::transport(
                                code::SHUTTING_DOWN,
                                "core_not_ready",
                                "daemon 已进入关闭序列",
                            )
                            .with_reason("shutting_down")
                            .into_error_object(),
                        )
                        .to_value(),
                    );
                }
                self.stage = Stage::ShuttingDown;
                let shutdown_id = server.shutdown_id();
                Handled::ReplyAndClose(
                    Response::result(
                        id.to_value(),
                        serde_json::json!({"accepted": true, "shutdown_id": shutdown_id}),
                    )
                    .to_value(),
                )
            }
            _ => match self.stage {
                Stage::AwaitingInitialize => Handled::Reply(
                    Response::error(
                        id.to_value(),
                        RpcFailure::core_not_ready(
                            "initialize 必须是连接上的第一个请求；请先完成握手",
                        )
                        .with_reason("not_initialized")
                        .into_error_object(),
                    )
                    .to_value(),
                ),
                Stage::ShuttingDown => Handled::Reply(
                    Response::error(
                        id.to_value(),
                        RpcFailure::transport(
                            code::SHUTTING_DOWN,
                            "core_not_ready",
                            "daemon 已进入关闭序列，拒绝新请求",
                        )
                        .with_reason("shutting_down")
                        .into_error_object(),
                    )
                    .to_value(),
                ),
                Stage::Ready => match server.call(&method, params) {
                    Ok(result) => {
                        Handled::Reply(Response::result(id.to_value(), result).to_value())
                    }
                    Err(failure) => Handled::Reply(
                        Response::error(id.to_value(), failure.into_error_object()).to_value(),
                    ),
                },
            },
        }
    }

    fn handle_initialize(&mut self, id: Id, params: &Value, server: &mut impl Server) -> Handled {
        if self.stage != Stage::AwaitingInitialize {
            return Handled::Reply(Self::invalid_request(
                id.to_value(),
                "invalid_value",
                "重复的 initialize（每个连接只允许一次）",
                "duplicate_initialize",
            ));
        }

        let params = match InitializeParams::from_value(params) {
            Ok(params) => params,
            Err(failure) => {
                return Handled::Reply(
                    Response::error(id.to_value(), failure.into_error_object()).to_value(),
                )
            }
        };
        if let Err(failure) = params.negotiate() {
            return Handled::Reply(
                Response::error(id.to_value(), failure.into_error_object()).to_value(),
            );
        }

        self.stage = Stage::Ready;
        server.initialize_accepted();
        let result = InitializeResult::new(
            server.daemon_version(),
            server.core_version(),
            server.data_dir(),
            server.phase(),
        );
        Handled::Reply(Response::result(id.to_value(), result.to_value()).to_value())
    }

    /// 超限帧（>2MiB）：只失败该请求，不改变连接状态（DoD-7）。
    pub fn oversized(&self, limit: usize, discarded: u64) -> Handled {
        Handled::Reply(
            Response::error(
                Value::Null,
                RpcFailure::transport(
                    code::TOO_LARGE,
                    "too_large",
                    format!("JSON-RPC 帧超过 {limit} 字节上限（已丢弃该帧 {discarded} 字节）"),
                )
                .with_reason("frame_too_large")
                .with_detail("limit", u64::try_from(limit).unwrap_or(u64::MAX))
                .with_detail("discarded", discarded)
                .into_error_object(),
            )
            .to_value(),
        )
    }

    fn invalid_request(
        id: Value,
        aether_code: &str,
        message: impl Into<String>,
        reason: &str,
    ) -> Value {
        Response::error(
            id,
            RpcFailure::transport(code::INVALID_REQUEST, aether_code, message)
                .with_reason(reason)
                .into_error_object(),
        )
        .to_value()
    }
}

/// 有界 id 去重窗口（FIFO 淘汰）。
struct IdWindow {
    capacity: usize,
    order: VecDeque<Id>,
    seen: HashSet<Id>,
}

impl IdWindow {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// 记录 id；重复返回 `false`。
    fn remember(&mut self, id: &Id) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.order.push_back(id.clone());
        self.seen.insert(id.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct FakeServer {
        // `DaemonPhase` 无 Default：用 `starting` 作为缺省（FakeServer::ready 覆盖）。
        phase: Option<DaemonPhase>,
        calls: Vec<(String, Value)>,
        initialized: u32,
    }

    impl FakeServer {
        fn ready() -> Self {
            Self {
                phase: Some(DaemonPhase::Warming),
                ..Self::default()
            }
        }
    }

    impl Server for FakeServer {
        fn phase(&self) -> DaemonPhase {
            self.phase.unwrap_or(DaemonPhase::Starting)
        }
        fn data_dir(&self) -> String {
            r"C:\Aether".to_owned()
        }
        fn daemon_version(&self) -> String {
            "0.1.0".to_owned()
        }
        fn core_version(&self) -> String {
            "0.1.0".to_owned()
        }
        fn initialize_accepted(&mut self) {
            self.initialized += 1;
        }
        fn shutdown_id(&mut self) -> String {
            "shutdown-1".to_owned()
        }
        fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcFailure> {
            if method == "unknown_method" {
                return Err(RpcFailure::method_not_found(method));
            }
            self.calls.push((method.to_owned(), params.clone()));
            Ok(json!({"ok": true, "method": method, "params": params}))
        }
    }

    fn initialize_line() -> &'static [u8] {
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1.0","client":"electron","clientCapabilities":["events"],"requiredCapabilities":["events"]}}"#
    }

    /// 第二次 `initialize`（不同 id：单独验证「重复 initialize」而非「重复 id」）。
    fn second_initialize_line() -> &'static [u8] {
        br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1.0"}}"#
    }

    fn reply(handled: Handled) -> Value {
        match handled {
            Handled::Reply(value) => value,
            Handled::ReplyAndClose(value) => value,
        }
    }

    #[test]
    fn initialize_is_the_first_request_and_starts_warming_reply() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let value = reply(connection.handle(initialize_line(), &mut server));
        assert_eq!(value["result"]["protocolVersion"], json!("1.0"));
        assert_eq!(value["result"]["phase"], json!("warming"));
        assert_eq!(value["result"]["daemonVersion"], json!("0.1.0"));
        assert_eq!(server.initialized, 1);
        assert!(connection.initialized());
    }

    #[test]
    fn business_request_before_initialize_is_core_not_ready() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let value = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":2,"method":"health"}"#,
            &mut server,
        ));
        assert_eq!(value["error"]["code"], json!(-32002));
        assert_eq!(
            value["error"]["data"]["aether_code"],
            json!("core_not_ready")
        );
        assert!(server.calls.is_empty(), "不得触达业务层");
    }

    #[test]
    fn duplicate_initialize_is_invalid_request() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let _ = reply(connection.handle(initialize_line(), &mut server));
        let value = reply(connection.handle(second_initialize_line(), &mut server));
        assert_eq!(value["error"]["code"], json!(-32600));
        assert_eq!(
            value["error"]["data"]["reason"],
            json!("duplicate_initialize")
        );
    }

    #[test]
    fn duplicate_request_id_is_invalid_request() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let _ = reply(connection.handle(initialize_line(), &mut server));
        let first = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":9,"method":"health"}"#,
            &mut server,
        ));
        assert_eq!(first["result"]["ok"], json!(true));
        let second = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":9,"method":"health"}"#,
            &mut server,
        ));
        assert_eq!(second["error"]["code"], json!(-32600));
        assert_eq!(
            second["error"]["data"]["reason"],
            json!("duplicate_request_id")
        );
        assert_eq!(server.calls.len(), 1, "重复 id 不得再次触达业务层");
    }

    #[test]
    fn parse_error_keeps_connection_usable() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let value = reply(connection.handle(b"{not json", &mut server));
        assert_eq!(value["error"]["code"], json!(-32700));
        assert_eq!(value["error"]["data"]["aether_code"], json!("invalid_json"));
        assert_eq!(value["id"], Value::Null);
        let value = reply(connection.handle(initialize_line(), &mut server));
        assert!(value.get("result").is_some(), "解析错误后连接可继续");
    }

    #[test]
    fn invalid_request_shapes_are_rejected() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        for raw in [
            br#"[1,2,3]"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1}"#,
            br#"{"jsonrpc":"1.0","id":1,"method":"health"}"#,
            br#"{"jsonrpc":"2.0","method":"health"}"#,
            br#"{"jsonrpc":"2.0","id":null,"method":"health"}"#,
            br#""scalar""#,
        ] {
            let value = reply(connection.handle(raw, &mut server));
            assert_eq!(value["error"]["code"], json!(-32600), "raw={raw:?}");
        }
        assert!(server.calls.is_empty());
    }

    #[test]
    fn unknown_method_maps_to_method_not_found() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let _ = reply(connection.handle(initialize_line(), &mut server));
        let value = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":3,"method":"unknown_method"}"#,
            &mut server,
        ));
        assert_eq!(value["error"]["code"], json!(-32601));
        assert_eq!(
            value["error"]["data"]["aether_code"],
            json!("not_implemented")
        );
    }

    #[test]
    fn oversized_frame_is_too_large_and_connection_survives() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let value = reply(connection.oversized(crate::MAX_FRAME_BYTES, 2_097_153));
        assert_eq!(value["error"]["code"], json!(-32003));
        assert_eq!(value["error"]["data"]["aether_code"], json!("too_large"));
        assert_eq!(value["error"]["data"]["details"]["limit"], json!(2_097_152));
        assert!(!connection.initialized(), "超限不改连接状态");
        let value = reply(connection.handle(initialize_line(), &mut server));
        assert!(value.get("result").is_some());
    }

    #[test]
    fn shutdown_replies_accepted_then_rejects_new_requests() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let value = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#,
            &mut server,
        ));
        assert_eq!(value["result"]["accepted"], json!(true));
        assert_eq!(value["result"]["shutdown_id"], json!("shutdown-1"));
        let value = reply(connection.handle(
            br#"{"jsonrpc":"2.0","id":5,"method":"health"}"#,
            &mut server,
        ));
        assert_eq!(value["error"]["code"], json!(-32004));
        assert_eq!(value["error"]["data"]["reason"], json!("shutting_down"));
    }

    #[test]
    fn shutdown_resets_state_machine_even_without_initialize() {
        let mut connection = Connection::new();
        let mut server = FakeServer::ready();
        let handled = connection.handle(
            br#"{"jsonrpc":"2.0","id":6,"method":"shutdown"}"#,
            &mut server,
        );
        assert!(matches!(handled, Handled::ReplyAndClose(_)));
        assert!(connection.shutting_down());
    }

    #[test]
    fn id_window_is_bounded_and_evicts_oldest() {
        let mut window = IdWindow::new(2);
        assert!(window.remember(&Id::Number(1)));
        assert!(window.remember(&Id::Number(2)));
        assert!(!window.remember(&Id::Number(1)), "窗口内重复");
        assert!(window.remember(&Id::Number(3)), "窗口满后淘汰最旧");
        assert!(window.remember(&Id::Number(1)), "已淘汰 id 可复用");
        assert_eq!(window.order.len(), 2);
    }
}
