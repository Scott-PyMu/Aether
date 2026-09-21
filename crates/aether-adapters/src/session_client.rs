//! 适配器会话客户端（M2-02）：核心侧对 D6 会话方法的封装。
//!
//! 封装 `createSession` / `sendMessage`（流式观察）/ `tools.list` / `permission.resolve` /
//! `interrupt` / `dispose` / `shutdown`，并把适配器上报的事件信封按 run 汇总为
//! [`RunOutcome`]。连接以 `Arc<AdapterConnection>` 共享（监督器 `connection()` 提供），
//! 通知由后台 pump 消费。
//!
//! 失败语义（D5「运行中崩溃」）：
//! - 连接断开时，所有在途（已 `run.started`、未收终态）run 标记
//!   [`RunOutcome::Disconnected`]（错误码 [`ADAPTER_DISCONNECTED_CODE`]）；核心据此落
//!   `run.failed` 行与事件（M2-01 生命周期/M3-06 `run_retry` 接线）；
//! - 单方法请求超时/错误按 D6 超时表返回 [`RequestError`]（不改变连接状态）。
//!
//! 注意：本模块不写存储、不发事件；只做「线协议 → 内存投影」。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aether_core::{ErrorInfo, EventEnvelope, EventPayload, EventType};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::connection::{AdapterConnection, AdapterNotification, ConnectionState, RequestError};
use crate::protocol::Method;

/// 连接在 run 在途时断开的错误码（核心据此将 run 标 failed）。
pub const ADAPTER_DISCONNECTED_CODE: &str = "adapter_disconnected";

/// 客户端事件缓冲上限（有界，防诊断无限增长；溢出计 `dropped_events`）。
pub const CLIENT_EVENT_LIMIT: usize = 4096;

/// run 终态（客户端投影）。
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// 收到 `run.completed`。
    Completed {
        /// 终稿正文（`message.completed.payload.message.content`）。
        assistant_text: Option<String>,
        /// usage（原样 JSON；形状见附录 B）。
        usage: Option<Value>,
    },
    /// 收到 `run.failed`。
    Failed {
        /// 错误信息（`run.failed.payload.error`）。
        error: ErrorInfo,
    },
    /// 收到 `run.cancelled`。
    Cancelled {
        /// 原因（`run.cancelled.payload.reason`）。
        reason: Option<String>,
    },
    /// 连接在 run 在途时断开（D5：核心据此标 failed）。
    Disconnected {
        /// 断开细节。
        detail: String,
    },
}

impl RunOutcome {
    /// 错误码（Failed/Disconnected 时有值；诊断与核心落库用）。
    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::Failed { error } => Some(error.code.as_str()),
            Self::Disconnected { .. } => Some(ADAPTER_DISCONNECTED_CODE),
            Self::Completed { .. } | Self::Cancelled { .. } => None,
        }
    }
}

/// 会话创建结果（`session.create` 响应 + Mode R 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    /// 适配器侧会话 id（本适配器 = 原生会话 id）。
    pub session_id: String,
    /// 原生会话 id（ADR-005：`sessions.config.native_id` 的来源）。
    pub native_id: Option<String>,
    /// 是否由 `native_id` 恢复（Mode R 重放路径）。
    pub resumed: bool,
}

/// `session.send` ack。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendAck {
    /// run id。
    pub run_id: String,
    /// 是否幂等命中（相同 `client_msg_id`）。
    pub duplicate: bool,
}

/// 工具定义（`tools.list` 条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// 工具名。
    pub name: String,
    /// 描述（可空）。
    pub description: Option<String>,
    /// 入参 schema（形状由适配器声明）。
    pub input_schema: Value,
}

/// 会话客户端错误。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SessionClientError {
    /// 请求失败（超时/断连/适配器错误）。
    #[error("适配器请求失败：{0}")]
    Request(#[from] RequestError),
    /// 响应形状不符（契约违约）。
    #[error("适配器响应形状不符：{0}")]
    Protocol(String),
}

#[derive(Debug, Default)]
struct ClientState {
    /// 已收终态的 run。
    outcomes: HashMap<String, RunOutcome>,
    /// 在途 run（`run.started` 后未收终态）。
    active: HashSet<String>,
    /// 各 run 的 `message.completed` 终稿正文（`run.completed` 到达前已入库）。
    message_texts: HashMap<String, String>,
    /// 全部事件（有界）。
    events: Vec<EventEnvelope>,
    /// 溢出丢弃计数。
    dropped_events: u64,
    /// `permission.request` 通知。
    permission_requests: Vec<Value>,
    /// `log` 通知。
    logs: Vec<Value>,
    /// 断连细节（`None` = 连接仍存活/未观察到断开）。
    disconnected: Option<String>,
}

/// 适配器会话客户端（`Arc<AdapterConnection>` 的投影包装）。
pub struct AdapterSessionClient {
    connection: Arc<AdapterConnection>,
    state: Arc<Mutex<ClientState>>,
    notify: Arc<Notify>,
    pump: JoinHandle<()>,
}

impl AdapterSessionClient {
    /// 建立客户端并启动通知 pump（连接须已握手完成）。
    pub fn new(connection: Arc<AdapterConnection>) -> Self {
        let state = Arc::new(Mutex::new(ClientState::default()));
        let notify = Arc::new(Notify::new());
        let pump = tokio::spawn(pump_notifications(
            Arc::clone(&connection),
            Arc::clone(&state),
            Arc::clone(&notify),
        ));
        Self {
            connection,
            state,
            notify,
            pump,
        }
    }

    /// 底层连接（诊断/高级用法）。
    pub fn connection(&self) -> &Arc<AdapterConnection> {
        &self.connection
    }

    /// `initialize`（D6 10s 超时）。
    pub async fn initialize(&self, config: Value) -> Result<Value, SessionClientError> {
        Ok(self
            .connection
            .request(Method::Initialize, json!({ "config": config }))
            .await?)
    }

    /// `session.create`（可选 `native_id` = Mode R 恢复）。
    pub async fn create_session(
        &self,
        title: Option<&str>,
        native_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<CreatedSession, SessionClientError> {
        let mut params = json!({});
        if let Some(title) = title {
            params["title"] = Value::String(title.to_owned());
        }
        if let Some(native_id) = native_id {
            params["native_id"] = Value::String(native_id.to_owned());
        }
        if let Some(model) = model {
            params["model"] = Value::String(model.to_owned());
        }
        let response = self
            .connection
            .request(Method::SessionCreate, params)
            .await?;
        let session_id = response["session_id"]
            .as_str()
            .ok_or_else(|| {
                SessionClientError::Protocol("session.create 缺少 session_id".to_owned())
            })?
            .to_owned();
        Ok(CreatedSession {
            session_id,
            native_id: response["native_id"].as_str().map(str::to_owned),
            resumed: response["resumed"].as_bool().unwrap_or(false),
        })
    }

    /// `session.send` ack（快路径：适配器提交后立即返回，不等模型）。
    pub async fn send(
        &self,
        session_id: &str,
        client_msg_id: &str,
        text: &str,
    ) -> Result<SendAck, SessionClientError> {
        let response = self
            .connection
            .request(
                Method::SessionSend,
                json!({
                    "session_id": session_id,
                    "client_msg_id": client_msg_id,
                    "text": text,
                }),
            )
            .await?;
        let run_id = response["run_id"]
            .as_str()
            .ok_or_else(|| SessionClientError::Protocol("session.send 缺少 run_id".to_owned()))?
            .to_owned();
        Ok(SendAck {
            run_id,
            duplicate: response["duplicate"].as_bool().unwrap_or(false),
        })
    }

    /// `session.interrupt`（D6 5s 超时）。
    pub async fn interrupt(&self, session_id: &str) -> Result<Value, SessionClientError> {
        Ok(self
            .connection
            .request(
                Method::SessionInterrupt,
                json!({ "session_id": session_id }),
            )
            .await?)
    }

    /// `session.dispose`（D6 15s 超时）。
    pub async fn dispose(&self, session_id: &str) -> Result<Value, SessionClientError> {
        Ok(self
            .connection
            .request(Method::SessionDispose, json!({ "session_id": session_id }))
            .await?)
    }

    /// `tools.list`（D6 10s 超时）。
    pub async fn tools_list(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<ToolDefinition>, SessionClientError> {
        let params = match session_id {
            Some(session_id) => json!({ "session_id": session_id }),
            None => json!({}),
        };
        let response = self.connection.request(Method::ToolsList, params).await?;
        let tools = response["tools"]
            .as_array()
            .ok_or_else(|| SessionClientError::Protocol("tools.list 缺少 tools 数组".to_owned()))?;
        let mut definitions = Vec::with_capacity(tools.len());
        for tool in tools {
            let name = tool["name"]
                .as_str()
                .ok_or_else(|| SessionClientError::Protocol("工具定义缺少 name".to_owned()))?;
            definitions.push(ToolDefinition {
                name: name.to_owned(),
                description: tool["description"].as_str().map(str::to_owned),
                input_schema: tool["input_schema"].clone(),
            });
        }
        Ok(definitions)
    }

    /// `permission.resolve`（D6 5s 超时；D9 回环的决议下发入口）。
    pub async fn resolve_permission(
        &self,
        request_id: &str,
        decision: &str,
        scope: Option<&str>,
    ) -> Result<Value, SessionClientError> {
        let mut params = json!({ "request_id": request_id, "decision": decision });
        if let Some(scope) = scope {
            params["scope"] = Value::String(scope.to_owned());
        }
        Ok(self
            .connection
            .request(Method::PermissionResolve, params)
            .await?)
    }

    /// `health.ping`（D6 5s 超时）。
    pub async fn health_ping(&self) -> Result<Value, SessionClientError> {
        Ok(self
            .connection
            .request(Method::HealthPing, json!({}))
            .await?)
    }

    /// `shutdown`（D6 5s 超时）。
    pub async fn shutdown(&self) -> Result<Value, SessionClientError> {
        Ok(self.connection.request(Method::Shutdown, json!({})).await?)
    }

    /// 等待 run 终态（超时返回 `None`）。
    pub async fn wait_run_outcome(&self, run_id: &str, timeout: Duration) -> Option<RunOutcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(outcome) = self.run_outcome(run_id).await {
                return Some(outcome);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let notified = self.notify.notified();
            // 注册等待前重查，避免丢失唤醒。
            if let Some(outcome) = self.run_outcome(run_id).await {
                return Some(outcome);
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return None;
            }
        }
    }

    /// 读取 run 终态（不等待）。
    pub async fn run_outcome(&self, run_id: &str) -> Option<RunOutcome> {
        self.state.lock().await.outcomes.get(run_id).cloned()
    }

    /// 等待 run 出现指定事件（含已收到的历史；超时 false）。
    pub async fn wait_for_event(
        &self,
        run_id: &str,
        event_type: EventType,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.has_event(run_id, event_type).await {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let notified = self.notify.notified();
            if self.has_event(run_id, event_type).await {
                return true;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return false;
            }
        }
    }

    async fn has_event(&self, run_id: &str, event_type: EventType) -> bool {
        let state = self.state.lock().await;
        state.events.iter().any(|event| {
            event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id)
                && event.event_type() == event_type
        })
    }

    /// run 的事件类型序列（到达顺序）。
    pub async fn run_event_types(&self, run_id: &str) -> Vec<String> {
        let state = self.state.lock().await;
        state
            .events
            .iter()
            .filter(|event| event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id))
            .map(|event| event.event_type().as_str().to_owned())
            .collect()
    }

    /// 全部事件快照（有界缓冲）。
    pub async fn events(&self) -> Vec<EventEnvelope> {
        self.state.lock().await.events.clone()
    }

    /// `permission.request` 通知快照（D9 回环证据）。
    pub async fn permission_requests(&self) -> Vec<Value> {
        self.state.lock().await.permission_requests.clone()
    }

    /// `log` 通知快照。
    pub async fn logs(&self) -> Vec<Value> {
        self.state.lock().await.logs.clone()
    }

    /// 事件缓冲溢出计数。
    pub async fn dropped_events(&self) -> u64 {
        self.state.lock().await.dropped_events
    }

    /// 等待观察到连接断开（超时 `None`）。
    pub async fn wait_disconnect(&self, timeout: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(detail) = self.disconnect_detail().await {
                return Some(detail);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let notified = self.notify.notified();
            if let Some(detail) = self.disconnect_detail().await {
                return Some(detail);
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return None;
            }
        }
    }

    /// 当前断连细节（未断开为 `None`）。
    pub async fn disconnect_detail(&self) -> Option<String> {
        self.state.lock().await.disconnected.clone()
    }
}

impl Drop for AdapterSessionClient {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// 通知 pump：事件路由 → 状态投影；断连（状态通道 `Disconnected` 或通知通道关闭）时
/// 收口在途 run。
///
/// 说明：进程被强杀时 stdout EOF，但连接的通知发送端由写任务持有，通知通道可能不立即
/// 关闭（M2-02 T5a 实测）；因此同时订阅连接状态，`Disconnected` 即触发收口。
async fn pump_notifications(
    connection: Arc<AdapterConnection>,
    state: Arc<Mutex<ClientState>>,
    notify: Arc<Notify>,
) {
    let mut state_rx = connection.subscribe_state();
    loop {
        tokio::select! {
            notification = connection.next_notification() => {
                let Some(notification) = notification else {
                    break;
                };
                {
                    let mut guard = state.lock().await;
                    match notification {
                        AdapterNotification::Event(envelope) => {
                            let envelope = *envelope;
                            let run_id = envelope.run_id.as_ref().map(|id| id.as_str().to_owned());
                            let event_type = envelope.event_type();
                            if let Some(run_id) = run_id.as_deref() {
                                match event_type {
                                    EventType::RunStarted => {
                                        guard.active.insert(run_id.to_owned());
                                    }
                                    EventType::MessageCompleted => {
                                        if let EventPayload::MessageCompleted(payload) = &envelope.payload {
                                            guard
                                                .message_texts
                                                .insert(run_id.to_owned(), payload.message.content.clone());
                                        }
                                    }
                                    EventType::RunCompleted => {
                                        let assistant_text = guard.message_texts.get(run_id).cloned();
                                        let usage = match &envelope.payload {
                                            EventPayload::RunCompleted(payload) => payload
                                                .usage
                                                .as_ref()
                                                .and_then(|usage| serde_json::to_value(usage).ok()),
                                            _ => None,
                                        };
                                        guard.active.remove(run_id);
                                        guard.outcomes.insert(
                                            run_id.to_owned(),
                                            RunOutcome::Completed {
                                                assistant_text,
                                                usage,
                                            },
                                        );
                                    }
                                    EventType::RunFailed => {
                                        let error = match &envelope.payload {
                                            EventPayload::RunFailed(payload) => payload.error.clone(),
                                            _ => ErrorInfo {
                                                code: "run_failed".to_owned(),
                                                message: "run.failed payload 形状异常".to_owned(),
                                                recoverable: true,
                                            },
                                        };
                                        guard.active.remove(run_id);
                                        guard
                                            .outcomes
                                            .insert(run_id.to_owned(), RunOutcome::Failed { error });
                                    }
                                    EventType::RunCancelled => {
                                        let reason = match &envelope.payload {
                                            EventPayload::RunCancelled(payload) => payload.reason.clone(),
                                            _ => None,
                                        };
                                        guard.active.remove(run_id);
                                        guard
                                            .outcomes
                                            .insert(run_id.to_owned(), RunOutcome::Cancelled { reason });
                                    }
                                    _ => {}
                                }
                            }
                            if guard.events.len() >= CLIENT_EVENT_LIMIT {
                                guard.events.remove(0);
                                guard.dropped_events += 1;
                            }
                            guard.events.push(envelope);
                        }
                        AdapterNotification::PermissionRequest(params) => {
                            guard.permission_requests.push(params)
                        }
                        AdapterNotification::Log(params) => guard.logs.push(params),
                        AdapterNotification::Other { .. } => {}
                    }
                }
                notify.notify_waiters();
            }
            changed = state_rx.changed() => {
                match changed {
                    Ok(()) => {
                        if matches!(*state_rx.borrow_and_update(), ConnectionState::Disconnected(_)) {
                            break;
                        }
                    }
                    // 状态通道关闭（reader/writer 任务释放）→ 连接终止。
                    Err(_) => break,
                }
            }
        }
    }

    let detail = format!("连接断开：{:?}", connection.state());
    let mut guard = state.lock().await;
    guard.disconnected = Some(detail.clone());
    let active: Vec<String> = guard.active.drain().collect();
    for run_id in active {
        guard.outcomes.insert(
            run_id,
            RunOutcome::Disconnected {
                detail: detail.clone(),
            },
        );
    }
    drop(guard);
    notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    /// 适配器侧对等端（测试桩）：发送通知 / 读取请求 / 回响应。
    struct Peer {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Peer {
        async fn send_value(&mut self, value: Value) {
            let line = serde_json::to_string(&value).expect("json");
            self.writer.write_all(line.as_bytes()).await.expect("write");
            self.writer.write_all(b"\n").await.expect("write lf");
            self.writer.flush().await.expect("flush");
        }

        async fn next_line(&mut self) -> Option<Value> {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).await.expect("read");
            if read == 0 {
                return None;
            }
            serde_json::from_str(line.trim()).ok()
        }

        async fn respond(&mut self, id: u64, result: Value) {
            self.send_value(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                .await;
        }

        async fn respond_error(&mut self, id: u64, code: i64, message: &str) {
            self.send_value(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message},
            }))
            .await;
        }

        /// 发送一条 `event` 通知（run_id 为 None 时不做 run 归属）。
        async fn notify_event(
            &mut self,
            seq: u64,
            run_id: Option<&str>,
            kind: &str,
            payload: Value,
        ) {
            self.send_value(json!({
                "jsonrpc": "2.0",
                "method": "event",
                "params": {
                    "v": 1,
                    "id": format!("01J{:023}", seq),
                    "session_id": "sess-1",
                    "run_id": run_id,
                    "runtime_id": "claude-code",
                    "seq": seq,
                    "ts": 1,
                    "type": kind,
                    "payload": payload,
                },
            }))
            .await;
        }

        /// 首帧 `hello`（D6：读侧要求首帧为 hello，否则协议违例断连）。
        async fn hello(&mut self) {
            self.send_value(json!({
                "jsonrpc": "2.0",
                "method": "hello",
                "params": {
                    "protocol": "1.0",
                    "runtime": {"name": "claude-code", "version": "0.1.0", "capabilities": []},
                },
            }))
            .await;
        }
    }

    fn setup() -> (AdapterSessionClient, Peer) {
        let (core_side, adapter_side) = tokio::io::duplex(1024 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter_side);
        let (core_read, core_write) = tokio::io::split(core_side);
        let connection = AdapterConnection::spawn(core_read, core_write);
        let client = AdapterSessionClient::new(Arc::new(connection));
        (
            client,
            Peer {
                reader: BufReader::new(adapter_read),
                writer: adapter_write,
            },
        )
    }

    fn usage_json() -> Value {
        json!({"input_tokens": 3, "output_tokens": 5, "total_tokens": 8})
    }

    #[tokio::test]
    async fn completed_run_projects_text_usage_and_event_log() {
        let (client, mut peer) = setup();
        peer.hello().await;
        peer.notify_event(1, Some("run-1"), "run.started", json!({"run_id": "run-1"}))
            .await;
        peer.notify_event(
            2,
            Some("run-1"),
            "message.delta",
            json!({"message_id": "m-1", "text": "hi"}),
        )
        .await;
        peer.notify_event(
            3,
            Some("run-1"),
            "message.completed",
            json!({
                "message": {
                    "id": "m-1", "session_id": "sess-1", "run_id": "run-1",
                    "role": "assistant", "content": "hi", "created_at": 1,
                },
                "usage": usage_json(),
            }),
        )
        .await;
        peer.notify_event(
            4,
            Some("run-1"),
            "run.completed",
            json!({"run_id": "run-1", "usage": usage_json()}),
        )
        .await;

        let outcome = client
            .wait_run_outcome("run-1", Duration::from_secs(2))
            .await
            .expect("终态");
        match outcome {
            RunOutcome::Completed {
                assistant_text,
                usage,
            } => {
                assert_eq!(assistant_text.as_deref(), Some("hi"));
                assert_eq!(usage, Some(usage_json()));
            }
            other => panic!("期望 Completed，实际 {other:?}"),
        }
        assert!(
            client
                .wait_for_event("run-1", EventType::MessageDelta, Duration::from_secs(1))
                .await
        );
        assert!(
            !client
                .wait_for_event("run-2", EventType::MessageDelta, Duration::from_millis(50))
                .await
        );
        assert_eq!(
            client.run_event_types("run-1").await,
            vec![
                "run.started",
                "message.delta",
                "message.completed",
                "run.completed"
            ]
        );
        assert!(matches!(
            client.connection().state(),
            crate::connection::ConnectionState::Ready(_)
        ));
    }

    #[tokio::test]
    async fn failed_and_cancelled_outcomes_carry_error_code() {
        let (client, mut peer) = setup();
        peer.hello().await;
        peer.notify_event(1, Some("run-f"), "run.started", json!({"run_id": "run-f"}))
            .await;
        peer.notify_event(
            2,
            Some("run-f"),
            "run.failed",
            json!({
                "run_id": "run-f",
                "error": {"code": "api_error", "message": "503", "recoverable": true},
            }),
        )
        .await;
        let failed = client
            .wait_run_outcome("run-f", Duration::from_secs(2))
            .await
            .expect("failed 终态");
        assert_eq!(failed.error_code(), Some("api_error"));

        peer.notify_event(3, Some("run-c"), "run.started", json!({"run_id": "run-c"}))
            .await;
        peer.notify_event(
            4,
            Some("run-c"),
            "run.cancelled",
            json!({"run_id": "run-c", "reason": "interrupted"}),
        )
        .await;
        let cancelled = client
            .wait_run_outcome("run-c", Duration::from_secs(2))
            .await
            .expect("cancelled 终态");
        match &cancelled {
            RunOutcome::Cancelled { reason } => assert_eq!(reason.as_deref(), Some("interrupted")),
            other => panic!("期望 Cancelled，实际 {other:?}"),
        }
        assert_eq!(cancelled.error_code(), None);
        // 未知 run：等待超时返回 None。
        assert!(client
            .wait_run_outcome("run-missing", Duration::from_millis(50))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn disconnect_marks_active_runs_failed_and_collects_side_channels() {
        let (client, mut peer) = setup();
        peer.hello().await;
        peer.notify_event(1, Some("run-a"), "run.started", json!({"run_id": "run-a"}))
            .await;
        peer.notify_event(
            2,
            Some("run-a"),
            "permission.requested",
            json!({
                "request_id": "01J0000000000000000000001",
                "resource": "fs.write",
                "action": "write",
                "target": "a.txt",
            }),
        )
        .await;
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "permission.request",
            "params": {"request_id": "01J0000000000000000000002", "resource": "fs.read"},
        }))
        .await;
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "log",
            "params": {"level": "info", "message": "hi"},
        }))
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.logs().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!client.logs().await.is_empty(), "log 通知必须入库");
        assert_eq!(client.permission_requests().await.len(), 1);
        assert_eq!(client.dropped_events().await, 0);

        // 断开：关闭适配器侧写端 → reader EOF → 状态 Disconnected。
        drop(peer);
        let outcome = client
            .wait_run_outcome("run-a", Duration::from_secs(2))
            .await
            .expect("断连收口");
        assert!(matches!(outcome, RunOutcome::Disconnected { .. }));
        assert_eq!(outcome.error_code(), Some(ADAPTER_DISCONNECTED_CODE));
        assert!(client
            .wait_disconnect(Duration::from_secs(2))
            .await
            .is_some());
        assert!(client.disconnect_detail().await.is_some());
    }

    #[tokio::test]
    async fn request_methods_map_responses_and_errors() {
        let (client, mut peer) = setup();
        let client = Arc::new(client);

        // initialize
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.initialize(json!({"k": 1})).await }
        });
        let request = peer.next_line().await.expect("initialize 请求");
        assert_eq!(request["method"], "initialize");
        peer.respond(
            request["id"].as_u64().unwrap(),
            json!({"acknowledged": true}),
        )
        .await;
        assert_eq!(task.await.unwrap().unwrap()["acknowledged"], true);

        // session.create（native_id 为空 → resumed=false；缺 session_id → Protocol 错误）
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_session(Some("t"), None, None).await }
        });
        let request = peer.next_line().await.expect("session.create");
        assert_eq!(request["params"]["title"], "t");
        peer.respond(
            request["id"].as_u64().unwrap(),
            json!({"session_id": "s-1", "native_id": "n-1", "resumed": false}),
        )
        .await;
        let created = task.await.unwrap().unwrap();
        assert_eq!(created.session_id, "s-1");
        assert_eq!(created.native_id.as_deref(), Some("n-1"));
        assert!(!created.resumed);

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.create_session(None, Some("n-1"), None).await }
        });
        let request = peer.next_line().await.expect("session.create resume");
        assert_eq!(request["params"]["native_id"], "n-1");
        peer.respond(request["id"].as_u64().unwrap(), json!({"resumed": true}))
            .await;
        assert!(matches!(
            task.await.unwrap(),
            Err(SessionClientError::Protocol(_))
        ));

        // session.send：正常 ack 与重复标记
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.send("s-1", "c-1", "hi").await }
        });
        let request = peer.next_line().await.expect("session.send");
        peer.respond(
            request["id"].as_u64().unwrap(),
            json!({"accepted": true, "run_id": "run-1", "duplicate": true}),
        )
        .await;
        let ack = task.await.unwrap().unwrap();
        assert!(ack.duplicate);
        assert_eq!(ack.run_id, "run-1");

        // session.send 缺 run_id → Protocol 错误
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.send("s-1", "c-2", "hi").await }
        });
        let request = peer.next_line().await.expect("session.send 2");
        peer.respond(request["id"].as_u64().unwrap(), json!({"accepted": true}))
            .await;
        assert!(matches!(
            task.await.unwrap(),
            Err(SessionClientError::Protocol(_))
        ));

        // tools.list：解析定义；缺数组 → Protocol 错误
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.tools_list(Some("s-1")).await }
        });
        let request = peer.next_line().await.expect("tools.list");
        assert_eq!(request["params"]["session_id"], "s-1");
        peer.respond(
            request["id"].as_u64().unwrap(),
            json!({"tools": [
                {"name": "Bash", "description": "shell", "input_schema": {"type": "object"}},
                {"name": "Read"},
            ]}),
        )
        .await;
        let tools = task.await.unwrap().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "Bash");
        assert_eq!(tools[1].description, None);

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.tools_list(None).await }
        });
        let request = peer.next_line().await.expect("tools.list 2");
        peer.respond(request["id"].as_u64().unwrap(), json!({}))
            .await;
        assert!(matches!(
            task.await.unwrap(),
            Err(SessionClientError::Protocol(_))
        ));

        // 其余方法透传 + RPC 错误码映射
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.interrupt("s-1").await }
        });
        let request = peer.next_line().await.expect("session.interrupt");
        peer.respond(
            request["id"].as_u64().unwrap(),
            json!({"interrupted": true}),
        )
        .await;
        assert_eq!(task.await.unwrap().unwrap()["interrupted"], true);

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.dispose("s-1").await }
        });
        let request = peer.next_line().await.expect("session.dispose");
        peer.respond(request["id"].as_u64().unwrap(), json!({"disposed": true}))
            .await;
        assert_eq!(task.await.unwrap().unwrap()["disposed"], true);

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.resolve_permission("r-1", "deny", Some("once")).await }
        });
        let request = peer.next_line().await.expect("permission.resolve");
        assert_eq!(request["params"]["scope"], "once");
        peer.respond_error(request["id"].as_u64().unwrap(), 1005, "会话不存在")
            .await;
        match task.await.unwrap() {
            Err(SessionClientError::Request(RequestError::Rpc(error))) => {
                assert_eq!(error.code, 1005)
            }
            other => panic!("错误类型不符: {other:?}"),
        }

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.health_ping().await }
        });
        let request = peer.next_line().await.expect("health.ping");
        peer.respond(request["id"].as_u64().unwrap(), json!({"status": "ok"}))
            .await;
        assert_eq!(task.await.unwrap().unwrap()["status"], "ok");

        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.shutdown().await }
        });
        let request = peer.next_line().await.expect("shutdown");
        peer.respond(request["id"].as_u64().unwrap(), json!({"ok": true}))
            .await;
        assert_eq!(task.await.unwrap().unwrap()["ok"], true);
    }
}
