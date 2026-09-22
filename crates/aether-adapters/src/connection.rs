//! 适配器连接：握手、请求/响应（D6 超时表）、通知分发、健康计数。
//!
//! 职责边界（M1-09）：
//! - 帧读写：`tokio-util` `Framed` + [`AetherLineCodec`]（先日志后广播与本层无关）；
//! - 握手：首帧必须为合法 `hello`，10s 超时，major 校验（D6）；
//! - 请求：按 [`Method::timeout`] 超时，超时映射应用码 1002；
//! - 健康：连续无效帧（坏 JSON / 帧校验失败）达 20 → 判不健康并断连（D6 失败场景表）；
//! - 大行：单帧硬上限 2MiB（超限不继续缓冲即断连）；`artifact_ref` 引用帧必须 <1MiB
//!   （1–2MiB 声称引用视为契约违约断连，D6）。
//!
//! 重启/退避/熔断等监督语义属 M1-10，本模块只暴露状态与原因。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aether_core::EventEnvelope;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::artifact::ArtifactRefParams;
use crate::framing::{AetherLineCodec, ChunkLimitedReader, FrameError, RawLine, READ_CHUNK_BYTES};
use crate::protocol::{
    code, notify, validate_hello, DisabledInfo, Hello, Method, INVALID_FRAME_UNHEALTHY_THRESHOLD,
};

/// 核心 → 适配器出站队列容量（D6：核心侧 `mpsc(256)`）。
pub const OUTBOUND_QUEUE_CAPACITY: usize = 256;
/// 通知入站队列容量（D8：有界队列，reader 永不阻塞，满则丢弃并计数）。
pub const NOTIFICATION_QUEUE_CAPACITY: usize = 1024;
/// 记录错误行数上限（诊断用）。
pub const RECORDED_ERRORS_LIMIT: usize = 64;

/// 适配器 → 核心通知。
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterNotification {
    /// `event`：事件信封（附录 B 类型，已在核心侧严格校验）。
    Event(Box<EventEnvelope>),
    /// `permission.request`：权限请求（D9 回环的适配器侧入口）。
    PermissionRequest(Value),
    /// `log`：日志通知。
    Log(Value),
    /// `artifact_ref`：附件引用帧（D6；M2-09 落全帧形状）。
    ///
    /// 数据体不进入线协议——引用帧只含路径 + 元数据；路径安全校验由
    /// [`crate::artifact::ArtifactValidator`] 在消费侧完成（本层只做形状校验）。
    ArtifactRef(Box<ArtifactRefParams>),
    /// 其它通知（前向兼容，未知字段/方法忽略，不断连）。
    Other { method: String, params: Value },
}

/// 连接断开原因（结构化，「断连记错」的诊断字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// 流关闭；`incomplete_line_bytes > 0` 表示半行/断流（D6）。
    StreamClosed { incomplete_line_bytes: usize },
    /// D6 契约违约：1–2MiB 帧声称 `artifact_ref`（引用帧必须 <1MiB）→ 断连。
    ArtifactRefContractViolation { bytes: usize },
    /// D6：单帧超过 2MiB。
    LineTooLong { limit: usize },
    /// D6：连续无效帧达到阈值（20）→ 判不健康。
    InvalidFrameStreak { count: u32, threshold: u32 },
    /// 协议违例（如首帧不是 `hello`）。
    ProtocolViolation { detail: String },
    /// IO 错误。
    Io { detail: String },
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StreamClosed {
                incomplete_line_bytes,
            } => write!(f, "流关闭（残行 {incomplete_line_bytes} 字节）"),
            Self::ArtifactRefContractViolation { bytes } => write!(
                f,
                "artifact_ref 引用帧越界（已缓冲 {bytes} 字节，契约上限 1MiB）→ 断连（D6）"
            ),
            Self::LineTooLong { limit } => write!(f, "单帧超过 {limit} 字节上限（D6）"),
            Self::InvalidFrameStreak { count, threshold } => {
                write!(
                    f,
                    "连续 {count} 次无效帧（阈值 {threshold}）→ 判不健康（D6）"
                )
            }
            Self::ProtocolViolation { detail } => write!(f, "协议违例: {detail}"),
            Self::Io { detail } => write!(f, "IO 错误: {detail}"),
        }
    }
}

/// 连接状态（`hello` 校验通过即 Ready；无效帧降级 Degraded，断连终态）。
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// 未收到合法 `hello`。
    Connecting,
    /// 握手完成。
    Ready(Hello),
    /// 出现无效帧但未达阈值（D6：跳过 + 诊断计数）。
    Degraded {
        hello: Hello,
        invalid_frame_streak: u32,
        last_error: String,
    },
    /// 连接终止（不可恢复，由 M1-10 决定重启）。
    Disconnected(DisconnectReason),
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// 错误码（标准码或应用码 1001–1005）。
    pub code: i64,
    /// 错误消息。
    pub message: String,
    /// 附加数据。
    pub data: Option<Value>,
}

/// 请求失败（核心侧口径）。
#[derive(Debug, Clone, PartialEq)]
pub enum RequestError {
    /// 方法超时（D6 超时表）。
    Timeout {
        /// 方法。
        method: Method,
        /// 生效超时。
        timeout: Duration,
    },
    /// 任意方法名请求超时（故障注入路径）。
    RawTimeout {
        /// 方法名。
        method: String,
        /// 生效超时。
        timeout: Duration,
    },
    /// 适配器返回 JSON-RPC 错误。
    Rpc(RpcError),
    /// 连接在响应前断开（含进程崩溃）。
    Disconnected {
        /// 细节。
        detail: String,
    },
    /// 出站队列不可用。
    QueueClosed {
        /// 细节。
        detail: String,
    },
}

impl RequestError {
    /// 映射到 D6 应用码：超时 1002；崩溃/断连 1001；其余透传。
    pub const fn code(&self) -> i64 {
        match self {
            Self::Timeout { .. } | Self::RawTimeout { .. } => code::REQUEST_TIMEOUT,
            Self::Disconnected { .. } | Self::QueueClosed { .. } => code::ADAPTER_CRASHED,
            Self::Rpc(error) => error.code,
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { method, timeout } => {
                write!(f, "请求 {method} 超时（{timeout:?}，错误码 1002）")
            }
            Self::RawTimeout { method, timeout } => {
                write!(f, "请求 {method} 超时（{timeout:?}，错误码 1002）")
            }
            Self::Rpc(error) => write!(f, "适配器返回错误 {}: {}", error.code, error.message),
            Self::Disconnected { detail } => write!(f, "连接断开: {detail}"),
            Self::QueueClosed { detail } => write!(f, "出站队列关闭: {detail}"),
        }
    }
}

impl std::error::Error for RequestError {}

enum Outbound {
    Frame(String),
    Close,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// 适配器连接（核心侧）。
pub struct AdapterConnection {
    outbound: mpsc::Sender<Outbound>,
    pending: PendingMap,
    /// 通知队列（M2-02：`&self` 取用——连接以 `Arc` 共享给会话客户端）。
    notifications: AsyncMutex<mpsc::Receiver<AdapterNotification>>,
    state: watch::Receiver<ConnectionState>,
    invalid_frames_total: Arc<AtomicU64>,
    invalid_frame_streak: Arc<AtomicU32>,
    dropped_notifications: Arc<AtomicU64>,
    recorded_errors: Arc<Mutex<Vec<String>>>,
    /// 首次解析成功的 `hello`（含其后降级/断连场景），握手竞态兜底。
    hello_seen: Arc<OnceLock<Hello>>,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl AdapterConnection {
    /// 从任意读写流建立连接（stdin/stdout、duplex、测试桩均可）。
    pub fn spawn<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::spawn_with_threshold(reader, writer, INVALID_FRAME_UNHEALTHY_THRESHOLD)
    }

    /// 自定义「连续无效帧」阈值（默认 20，D6 硬阈值；测试可调小）。
    pub fn spawn_with_threshold<R, W>(reader: R, writer: W, invalid_frame_threshold: u32) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (outbound, outbound_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let (notifications_tx, notifications) = mpsc::channel(NOTIFICATION_QUEUE_CAPACITY);
        let (state_tx, state) = watch::channel(ConnectionState::Connecting);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let invalid_frames_total = Arc::new(AtomicU64::new(0));
        let invalid_frame_streak = Arc::new(AtomicU32::new(0));
        let dropped_notifications = Arc::new(AtomicU64::new(0));
        let recorded_errors = Arc::new(Mutex::new(Vec::new()));
        let hello_seen = Arc::new(OnceLock::new());

        let reader_ctx = Arc::new(ReaderContext {
            state_tx,
            notifications_tx,
            pending: Arc::clone(&pending),
            outbound: outbound.clone(),
            invalid_frames_total: Arc::clone(&invalid_frames_total),
            invalid_frame_streak: Arc::clone(&invalid_frame_streak),
            dropped_notifications: Arc::clone(&dropped_notifications),
            recorded_errors: Arc::clone(&recorded_errors),
            hello_seen: Arc::clone(&hello_seen),
            invalid_frame_threshold,
        });

        // 读侧限流：保证缓冲上限 = 2MiB 帧上限 + READ_CHUNK_BYTES（D6）。
        let limited_reader = ChunkLimitedReader::new(reader, READ_CHUNK_BYTES);
        let framed_read = FramedRead::new(limited_reader, AetherLineCodec::default());
        let reader_task = tokio::spawn(read_loop(framed_read, Arc::clone(&reader_ctx)));

        let framed_write = FramedWrite::new(writer, AetherLineCodec::default());
        let writer_task = tokio::spawn(write_loop(framed_write, outbound_rx, reader_ctx));

        Self {
            outbound,
            pending,
            notifications: AsyncMutex::new(notifications),
            state,
            invalid_frames_total,
            invalid_frame_streak,
            dropped_notifications,
            recorded_errors,
            hello_seen,
            next_id: AtomicU64::new(1),
            reader_task,
            writer_task,
        }
    }

    /// 握手：等待 `hello`（10s 超时），major 不匹配返回 `Disabled` 信息（D6/DoD4）。
    ///
    /// 竞态兜底（CI 三平台矩阵暴露）：reader 可能在握手方观察到 `Ready` 之前就把状态
    /// 推进到 `Degraded`（hello 后混入无效帧）或 `Disconnected`（hello 后契约违约）——
    /// 只要 `hello` 曾按首帧解析成功，握手即视为成功；连接健康度由调用方按状态处理。
    pub async fn handshake(&self) -> Result<Hello, DisabledInfo> {
        self.handshake_with_timeout(crate::protocol::HANDSHAKE_TIMEOUT)
            .await
    }

    /// 自定义握手超时（测试用；生产固定 10s）。
    pub async fn handshake_with_timeout(&self, timeout: Duration) -> Result<Hello, DisabledInfo> {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let current = state.borrow_and_update().clone();
                match current {
                    ConnectionState::Ready(hello) => return Ok(hello),
                    ConnectionState::Degraded { hello, .. } => return Ok(hello),
                    ConnectionState::Disconnected(reason) => {
                        return match self.recorded_hello() {
                            Some(hello) => Ok(hello),
                            None => Err(DisabledInfo::protocol_error(format!(
                                "连接在握手完成前断开: {reason}"
                            ))),
                        };
                    }
                    ConnectionState::Connecting => {}
                }
                if state.changed().await.is_err() {
                    return match self.recorded_hello() {
                        Some(hello) => Ok(hello),
                        None => Err(DisabledInfo::protocol_error("连接状态通道关闭")),
                    };
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(hello)) => validate_hello(&hello).map(|()| hello),
            Ok(Err(disabled)) => Err(disabled),
            Err(_) => match self.recorded_hello() {
                Some(hello) => validate_hello(&hello).map(|()| hello),
                None => Err(DisabledInfo::handshake_timeout(timeout)),
            },
        }
    }

    /// 首次解析成功的 `hello`（即使连接其后降级/断连也不丢失）。
    fn recorded_hello(&self) -> Option<Hello> {
        self.hello_seen.get().cloned()
    }

    /// 按 D6 方法表超时发送请求。
    pub async fn request(&self, method: Method, params: Value) -> Result<Value, RequestError> {
        self.request_with_timeout(method, params, method.timeout())
            .await
    }

    /// 自定义超时发送请求（测试用）。
    pub async fn request_with_timeout(
        &self,
        method: Method,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        match self.request_raw(method.as_str(), params, timeout).await {
            Err(RequestError::RawTimeout { timeout, .. }) => {
                Err(RequestError::Timeout { method, timeout })
            }
            other => other,
        }
    }

    /// 以任意方法名发送请求（故障注入：未知方法必须回 `-32601` 且不断连）。
    pub async fn request_raw(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        if let ConnectionState::Disconnected(reason) = self.state.borrow().clone() {
            return Err(RequestError::Disconnected {
                detail: reason.to_string(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| RequestError::QueueClosed {
                detail: "pending 表锁中毒".to_owned(),
            })?;
            pending.insert(id, sender);
        }
        // 插入后复查：读循环可能在两步之间完成断连清理（避免请求悬挂到超时）。
        if let ConnectionState::Disconnected(reason) = self.state.borrow().clone() {
            self.remove_pending(id);
            return Err(RequestError::Disconnected {
                detail: reason.to_string(),
            });
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        if self.outbound.send(Outbound::Frame(frame)).await.is_err() {
            self.remove_pending(id);
            return Err(RequestError::Disconnected {
                detail: "写队列已关闭（适配器进程可能已退出）".to_owned(),
            });
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_error))) => Err(RequestError::Rpc(rpc_error)),
            Ok(Err(_)) => Err(RequestError::Disconnected {
                detail: "响应通道关闭（连接断开）".to_owned(),
            }),
            Err(_) => {
                self.remove_pending(id);
                Err(RequestError::RawTimeout {
                    method: method.to_owned(),
                    timeout,
                })
            }
        }
    }

    fn remove_pending(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// 取下一条通知（无通知且连接关闭时返回 `None`）。
    ///
    /// `&self`（M2-02）：连接以 `Arc` 共享时仍可消费通知；队列由异步互斥保护。
    pub async fn next_notification(&self) -> Option<AdapterNotification> {
        self.notifications.lock().await.recv().await
    }

    /// 非阻塞取通知（`&self`；队列被占用时返回 `None`）。
    pub fn try_next_notification(&self) -> Option<AdapterNotification> {
        let mut receiver = self.notifications.try_lock().ok()?;
        receiver.try_recv().ok()
    }

    /// 当前状态快照。
    pub fn state(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    /// 状态订阅（M1-10 监督器用）。
    pub fn subscribe_state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    /// 累计无效帧数。
    pub fn invalid_frames_total(&self) -> u64 {
        self.invalid_frames_total.load(Ordering::SeqCst)
    }

    /// 当前连续无效帧数（成功帧清零）。
    pub fn invalid_frame_streak(&self) -> u32 {
        self.invalid_frame_streak.load(Ordering::SeqCst)
    }

    /// 因通知队列满而丢弃的通知数（D8：reader 不阻塞）。
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped_notifications.load(Ordering::SeqCst)
    }

    /// 已记录错误（诊断用，保序、上限 [`RECORDED_ERRORS_LIMIT`]）。
    pub fn recorded_errors(&self) -> Vec<String> {
        match self.recorded_errors.lock() {
            Ok(errors) => errors.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// 等待进入 Disconnected 状态（测试/监督器用）。
    pub async fn wait_for_disconnect(&self, timeout: Duration) -> Option<DisconnectReason> {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let current = state.borrow_and_update().clone();
                if let ConnectionState::Disconnected(reason) = current {
                    return reason;
                }
                if state.changed().await.is_err() {
                    return DisconnectReason::Io {
                        detail: "状态通道关闭".to_owned(),
                    };
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.ok()
    }

    /// 请求优雅关闭（写侧 flush 后结束）。
    pub fn close(&self) {
        let _ = self.outbound.try_send(Outbound::Close);
    }
}

impl Drop for AdapterConnection {
    fn drop(&mut self) {
        let _ = self.outbound.try_send(Outbound::Close);
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

struct ReaderContext {
    state_tx: watch::Sender<ConnectionState>,
    notifications_tx: mpsc::Sender<AdapterNotification>,
    pending: PendingMap,
    outbound: mpsc::Sender<Outbound>,
    invalid_frames_total: Arc<AtomicU64>,
    invalid_frame_streak: Arc<AtomicU32>,
    dropped_notifications: Arc<AtomicU64>,
    recorded_errors: Arc<Mutex<Vec<String>>>,
    hello_seen: Arc<OnceLock<Hello>>,
    invalid_frame_threshold: u32,
}

impl ReaderContext {
    fn set_state(&self, state: ConnectionState) {
        // 首次 hello 记录：握手方可能错过 `Ready` 窗口（状态很快推进到 Degraded/断连）。
        if let ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } = &state {
            let _ = self.hello_seen.set(hello.clone());
        }
        let _ = self.state_tx.send(state);
    }

    fn current_hello(&self) -> Option<Hello> {
        match self.state_tx.borrow().clone() {
            ConnectionState::Ready(hello) => Some(hello),
            ConnectionState::Degraded { hello, .. } => Some(hello),
            ConnectionState::Connecting | ConnectionState::Disconnected(_) => None,
        }
    }

    fn record_error(&self, detail: String) {
        if let Ok(mut errors) = self.recorded_errors.lock() {
            if errors.len() >= RECORDED_ERRORS_LIMIT {
                errors.remove(0);
            }
            errors.push(detail);
        }
    }

    /// 记录无效帧；返回 `true` 表示仍健康（未达阈值）。
    fn record_invalid_frame(&self, detail: String) -> bool {
        let total = self.invalid_frames_total.fetch_add(1, Ordering::SeqCst) + 1;
        let streak = self.invalid_frame_streak.fetch_add(1, Ordering::SeqCst) + 1;
        self.record_error(format!("无效帧 #{total}（连续 {streak}）: {detail}"));
        if streak >= self.invalid_frame_threshold {
            self.set_state(ConnectionState::Disconnected(
                DisconnectReason::InvalidFrameStreak {
                    count: streak,
                    threshold: self.invalid_frame_threshold,
                },
            ));
            self.record_error(format!(
                "连续 {streak} 次无效帧 → 判不健康（阈值 {}，D6）",
                self.invalid_frame_threshold
            ));
            false
        } else {
            if let Some(hello) = self.current_hello() {
                self.set_state(ConnectionState::Degraded {
                    hello,
                    invalid_frame_streak: streak,
                    last_error: detail,
                });
            }
            true
        }
    }

    fn clear_invalid_streak(&self) {
        if self.invalid_frame_streak.swap(0, Ordering::SeqCst) > 0 {
            if let Some(hello) = self.current_hello() {
                self.set_state(ConnectionState::Ready(hello));
            }
        }
    }

    fn protocol_violation(&self, detail: String) {
        self.record_error(format!("协议违例: {detail}"));
        self.set_state(ConnectionState::Disconnected(
            DisconnectReason::ProtocolViolation { detail },
        ));
    }

    fn disconnect(&self, reason: DisconnectReason) {
        self.record_error(format!("断连: {reason}"));
        self.set_state(ConnectionState::Disconnected(reason));
    }

    fn fail_pending(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }

    fn push_notification(&self, notification: AdapterNotification) {
        if self.notifications_tx.try_send(notification).is_err() {
            self.dropped_notifications.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn resolve_response(&self, id: u64, value: &Value) {
        let sender = match self.pending.lock() {
            Ok(mut pending) => pending.remove(&id),
            Err(_) => None,
        };
        let Some(sender) = sender else {
            self.record_error(format!("收到未知响应 id={id}"));
            return;
        };
        let outcome = if let Some(error) = value.get("error") {
            Err(RpcError {
                code: error
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or(code::INTERNAL_ERROR),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("适配器返回未命名错误")
                    .to_owned(),
                data: error.get("data").cloned(),
            })
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = sender.send(outcome);
    }

    fn reply_method_not_found(&self, id: &Value, method: &str) {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code::METHOD_NOT_FOUND,
                "message": format!("Method not found: {method}"),
            },
        })
        .to_string();
        if self.outbound.try_send(Outbound::Frame(frame)).is_err() {
            self.record_error("回 -32601 失败：出站队列满或已关闭".to_owned());
        }
    }
}

async fn read_loop<R>(mut framed: FramedRead<R, AetherLineCodec>, ctx: Arc<ReaderContext>)
where
    R: AsyncRead + Unpin,
{
    loop {
        match framed.next().await {
            Some(Ok(raw)) => {
                if !dispatch_frame(raw, &ctx).await {
                    break;
                }
            }
            Some(Err(FrameError::InvalidUtf8 { .. })) => {
                // 行已被消费：按「无效帧」计数，可继续（D6 失败场景表）。
                if !ctx.record_invalid_frame("帧不是合法 UTF-8".to_owned()) {
                    break;
                }
            }
            Some(Err(FrameError::ArtifactRefContractViolation { bytes, .. })) => {
                ctx.disconnect(DisconnectReason::ArtifactRefContractViolation { bytes });
                break;
            }
            Some(Err(FrameError::LineTooLong { limit })) => {
                ctx.disconnect(DisconnectReason::LineTooLong { limit });
                break;
            }
            Some(Err(FrameError::IncompleteLine { bytes })) => {
                ctx.disconnect(DisconnectReason::StreamClosed {
                    incomplete_line_bytes: bytes,
                });
                break;
            }
            Some(Err(error)) => {
                ctx.disconnect(DisconnectReason::Io {
                    detail: error.to_string(),
                });
                break;
            }
            None => {
                ctx.disconnect(DisconnectReason::StreamClosed {
                    incomplete_line_bytes: 0,
                });
                break;
            }
        }
    }
    ctx.fail_pending();
}

async fn dispatch_frame(raw: RawLine, ctx: &ReaderContext) -> bool {
    let value: Value = match serde_json::from_str(&raw.text) {
        Ok(value) => value,
        Err(error) => {
            return ctx.record_invalid_frame(format!("JSON 解析失败: {error}"));
        }
    };
    let Some(object) = value.as_object() else {
        return ctx.record_invalid_frame("帧不是 JSON 对象".to_owned());
    };
    let id = object.get("id").cloned().filter(|value| !value.is_null());
    let method = object.get("method").and_then(Value::as_str);

    match (id, method) {
        (Some(id), None) => {
            // 响应：本连接只接受数字 id。
            if !object.contains_key("result") && !object.contains_key("error") {
                return ctx.record_invalid_frame("帧既无 method 也无 result/error".to_owned());
            }
            match id.as_u64() {
                Some(numeric) => {
                    ctx.resolve_response(numeric, &value);
                    ctx.clear_invalid_streak();
                    true
                }
                None => ctx.record_invalid_frame("响应 id 不是无符号整数".to_owned()),
            }
        }
        (Some(id), Some(name)) => {
            // 适配器 → 核心请求：MVP 无反向方法，回 -32601 不断连（D6 前向兼容）。
            ctx.reply_method_not_found(&id, name);
            ctx.clear_invalid_streak();
            true
        }
        (None, Some(name)) => dispatch_notification(name, object, ctx),
        (None, None) => ctx.record_invalid_frame("帧既无 id 也无 method".to_owned()),
    }
}

fn dispatch_notification(
    name: &str,
    object: &serde_json::Map<String, Value>,
    ctx: &ReaderContext,
) -> bool {
    // 首帧必须是 hello（D6：进程启动 10s 内必须发 hello）。
    if ctx.current_hello().is_none() && name != notify::HELLO {
        ctx.protocol_violation(format!("首帧必须为 hello，实际为 {name}"));
        return false;
    }
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match name {
        notify::HELLO => {
            if ctx.current_hello().is_some() {
                // 重复 hello：宽容处理（前向兼容），不视为无效帧。
                ctx.clear_invalid_streak();
                return true;
            }
            match serde_json::from_value::<Hello>(params) {
                Ok(hello) => {
                    ctx.set_state(ConnectionState::Ready(hello));
                    ctx.clear_invalid_streak();
                }
                Err(error) => {
                    return ctx.record_invalid_frame(format!("hello 校验失败: {error}"));
                }
            }
        }
        notify::EVENT => match serde_json::from_value::<EventEnvelope>(params) {
            Ok(envelope) => {
                ctx.push_notification(AdapterNotification::Event(Box::new(envelope)));
                ctx.clear_invalid_streak();
            }
            Err(error) => {
                return ctx.record_invalid_frame(format!("event 信封校验失败: {error}"));
            }
        },
        notify::PERMISSION_REQUEST => {
            ctx.push_notification(AdapterNotification::PermissionRequest(params));
            ctx.clear_invalid_streak();
        }
        notify::LOG => {
            ctx.push_notification(AdapterNotification::Log(params));
            ctx.clear_invalid_streak();
        }
        notify::ARTIFACT_REF => {
            // 形状校验失败按「无效帧」计数（跳过 + 诊断；D6 失败场景表与坏 JSON 同口径），
            // 不断连——路径安全校验在消费侧（ArtifactValidator）完成。
            match serde_json::from_value::<ArtifactRefParams>(params) {
                Ok(artifact_ref) => {
                    ctx.push_notification(AdapterNotification::ArtifactRef(Box::new(artifact_ref)));
                    ctx.clear_invalid_streak();
                }
                Err(error) => {
                    return ctx
                        .record_invalid_frame(format!("artifact_ref 引用帧校验失败: {error}"));
                }
            }
        }
        other => {
            ctx.push_notification(AdapterNotification::Other {
                method: other.to_owned(),
                params,
            });
            ctx.clear_invalid_streak();
        }
    }
    true
}

async fn write_loop<W>(
    mut sink: FramedWrite<W, AetherLineCodec>,
    mut rx: mpsc::Receiver<Outbound>,
    ctx: Arc<ReaderContext>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(item) = rx.recv().await {
        match item {
            Outbound::Frame(line) => {
                if let Err(error) = sink.send(line).await {
                    ctx.record_error(format!("出站写失败: {error}"));
                    if !matches!(
                        ctx.state_tx.borrow().clone(),
                        ConnectionState::Disconnected(_)
                    ) {
                        ctx.disconnect(DisconnectReason::Io {
                            detail: error.to_string(),
                        });
                    }
                    break;
                }
            }
            Outbound::Close => break,
        }
    }
    let _ = sink.close().await;
    ctx.fail_pending();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::ARTIFACT_REF_LIMIT;
    use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

    const HELLO_JSON: &str = r#"{"jsonrpc":"2.0","method":"hello","params":{"protocol":"1.0","runtime":{"name":"stub","version":"0.1.0","capabilities":["tools.list"]}}}"#;

    struct Peer {
        lines: FramedRead<ReadHalf<DuplexStream>, AetherLineCodec>,
        writer: Option<WriteHalf<DuplexStream>>,
    }

    impl Peer {
        fn new(stream: DuplexStream) -> Self {
            let (read, write) = tokio::io::split(stream);
            Self {
                lines: FramedRead::new(read, AetherLineCodec::default()),
                writer: Some(write),
            }
        }

        /// 发送一整行（自动补 LF，等价于适配器 SDK 的出站编码）。
        async fn send_line(&mut self, line: &str) {
            self.send_bytes(format!("{line}\n").as_bytes()).await;
        }

        /// 发送原始字节（不补 LF；用于半行/大行注入）。
        async fn send_bytes(&mut self, bytes: &[u8]) {
            let writer = self.writer.as_mut().expect("写侧已被取走");
            writer.write_all(bytes).await.unwrap();
            writer.flush().await.unwrap();
        }

        /// 取走写侧（用于会阻塞的超大行写入，调用方自行 abort）。
        fn take_writer(&mut self) -> Option<WriteHalf<DuplexStream>> {
            self.writer.take()
        }

        async fn send_value(&mut self, value: Value) {
            self.send_line(&value.to_string()).await;
        }

        async fn next_value(&mut self) -> Value {
            let raw = self.lines.next().await.unwrap().unwrap();
            serde_json::from_str(&raw.text).unwrap()
        }

        async fn hello(&mut self) {
            self.send_line(HELLO_JSON).await;
        }

        async fn respond(&mut self, id: u64, result: Value) {
            self.send_value(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                .await;
        }
    }

    fn connected(threshold: u32) -> (AdapterConnection, Peer) {
        let (core_side, peer_side) = tokio::io::duplex(256 * 1024);
        let (read, write) = tokio::io::split(core_side);
        (
            AdapterConnection::spawn_with_threshold(read, write, threshold),
            Peer::new(peer_side),
        )
    }

    async fn wait_until<F: Fn() -> bool>(condition: F) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("等待条件超时");
    }

    /// 等待无效帧计数达到期望（返回 bool，供断言而非 panic）。
    async fn wait_for_count(conn: &AdapterConnection, expected: u64, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if conn.invalid_frames_total() >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        conn.invalid_frames_total() >= expected
    }

    #[tokio::test]
    async fn handshake_ready_and_request_roundtrip() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        let hello = conn
            .handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(hello.protocol, "1.0");
        assert_eq!(hello.runtime.name, "stub");
        assert!(hello.runtime.has_capability("tools.list"));

        let (result, ()) = tokio::join!(conn.request(Method::HealthPing, json!({})), async {
            let request = peer.next_value().await;
            assert_eq!(request["method"], "health.ping");
            assert_eq!(request["jsonrpc"], "2.0");
            let id = request["id"].as_u64().unwrap();
            peer.respond(id, json!({"status": "ok"})).await;
        });
        assert_eq!(result.unwrap()["status"], "ok");
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
    }

    #[tokio::test]
    async fn handshake_timeout_is_disabled_with_handshake_timeout_reason() {
        let (conn, _peer) = connected(20);
        let error = conn
            .handshake_with_timeout(Duration::from_millis(60))
            .await
            .unwrap_err();
        assert_eq!(error.status, aether_core::RuntimeStatus::Disabled);
        assert_eq!(error.status_reason.as_str(), "handshake_timeout");
        assert!(error.upgrade_hint.is_none());
    }

    #[tokio::test]
    async fn handshake_succeeds_when_ready_degrades_before_observation() {
        // CI 三平台矩阵（macOS 时序）暴露的竞态：hello 后紧跟无效帧，reader 可能在
        // 握手方观察到 `Ready` 前就把状态推进到 `Degraded`；只认 `Ready` 会误报
        // handshake_timeout（D6：stdout 混入日志不得推翻已到达的 hello）。
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        peer.send_line("[info] 适配器日志误入 stdout").await;
        wait_until(|| matches!(conn.state(), ConnectionState::Degraded { .. })).await;

        let hello = conn
            .handshake_with_timeout(Duration::from_millis(500))
            .await
            .expect("hello 已到达，其后无效帧不得推翻握手");
        assert_eq!(hello.runtime.name, "stub");
        assert_eq!(hello.protocol, "1.0");
    }

    #[tokio::test]
    async fn handshake_succeeds_when_disconnect_follows_hello() {
        // 阈值 1：hello 后第一条无效帧即断连；hello 仍需被握手方取回（首帧契约达成）。
        let (conn, mut peer) = connected(1);
        peer.hello().await;
        peer.send_line("not json").await;
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(2))
            .await
            .expect("阈值 1 应立即断连");
        assert!(matches!(
            reason,
            DisconnectReason::InvalidFrameStreak { .. }
        ));

        let hello = conn
            .handshake_with_timeout(Duration::from_millis(500))
            .await
            .expect("断连发生在 hello 之后，握手应成功");
        assert_eq!(hello.runtime.name, "stub");
    }

    #[tokio::test]
    async fn version_mismatch_hello_disables_with_reason_and_upgrade_hint() {
        let (conn, mut peer) = connected(20);
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "hello",
            "params": {"protocol": "2.0", "runtime": {"name": "stub", "version": "9.9.9"}},
        }))
        .await;
        let error = conn
            .handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(error.status, aether_core::RuntimeStatus::Disabled);
        assert_eq!(error.status_reason.as_str(), "version_mismatch");
        let hint = error.upgrade_hint.clone().unwrap_or_default();
        assert!(hint.contains("2.0"), "升级提示需包含实际版本：{hint}");
        assert!(hint.contains("升级"), "升级提示需给出动作：{hint}");
    }

    #[tokio::test]
    async fn first_frame_not_hello_is_protocol_violation() {
        let (conn, mut peer) = connected(20);
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "log",
            "params": {"level": "info", "message": "早期日志"},
        }))
        .await;
        let error = conn
            .handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(error.status_reason.as_str(), "protocol_error");
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(2))
            .await
            .expect("应断连");
        assert!(matches!(reason, DisconnectReason::ProtocolViolation { .. }));
    }

    #[tokio::test]
    async fn twenty_consecutive_invalid_frames_mark_unhealthy() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();

        // 混入 stdout 日志（非 JSON）与坏 JSON：同属「帧校验失败」（D6）。
        for index in 0..19 {
            if index % 2 == 0 {
                peer.send_line("[info] 适配器日志误入 stdout").await;
            } else {
                peer.send_line("{ this is not json").await;
            }
        }
        wait_until(|| conn.invalid_frame_streak() == 19).await;
        assert_eq!(conn.invalid_frames_total(), 19);
        match conn.state() {
            ConnectionState::Degraded {
                invalid_frame_streak,
                ..
            } => assert_eq!(invalid_frame_streak, 19),
            other => panic!("19 次无效帧应处于 Degraded，实际 {other:?}"),
        }

        peer.send_line("still not json").await;
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(2))
            .await
            .expect("第 20 次应判不健康并断连");
        match reason {
            DisconnectReason::InvalidFrameStreak { count, threshold } => {
                assert_eq!(count, 20);
                assert_eq!(threshold, 20);
            }
            other => panic!("断连原因不符: {other:?}"),
        }
        assert_eq!(conn.invalid_frames_total(), 20);
        assert!(!conn.recorded_errors().is_empty(), "断连必须记错");
    }

    #[tokio::test]
    async fn valid_frame_resets_invalid_streak() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        for _ in 0..5 {
            peer.send_line("[warn] 误入 stdout 的日志").await;
        }
        wait_until(|| conn.invalid_frame_streak() == 5).await;
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "log",
            "params": {"level": "info", "message": "恢复"},
        }))
        .await;
        wait_until(|| conn.invalid_frame_streak() == 0).await;
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
    }

    #[tokio::test]
    async fn adapter_request_with_unknown_method_gets_32601_and_stays_connected() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "adapter.unknown",
            "params": {},
        }))
        .await;
        let reply = peer.next_value().await;
        assert_eq!(reply["id"], 77);
        assert_eq!(reply["error"]["code"], code::METHOD_NOT_FOUND);
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));

        // 连接未断：还能正常收发。
        let (result, ()) = tokio::join!(conn.request(Method::HealthPing, json!({})), async {
            let request = peer.next_value().await;
            let id = request["id"].as_u64().unwrap();
            peer.respond(id, json!({"status": "ok"})).await;
        });
        assert_eq!(result.unwrap()["status"], "ok");
    }

    #[tokio::test]
    async fn request_timeout_maps_to_application_code_1002() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let error = conn
            .request_with_timeout(Method::Initialize, json!({}), Duration::from_millis(60))
            .await
            .unwrap_err();
        assert!(matches!(error, RequestError::Timeout { .. }));
        assert_eq!(error.code(), 1002);
    }

    #[tokio::test]
    async fn disconnect_fails_pending_request_with_application_code_1001() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let request =
            conn.request_with_timeout(Method::HealthPing, json!({}), Duration::from_secs(5));
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("不应提前完成: {result:?}"),
            () = async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                drop(peer);
            } => {}
        }
        let error = request.await.unwrap_err();
        assert_eq!(error.code(), 1001);
    }

    #[tokio::test]
    async fn non_artifact_line_between_1_and_2_mib_parses_and_connection_survives() {
        // DoD3：1–2MiB 非引用行正常解析（不再按大行拒绝）。
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let padding = "x".repeat(1536 * 1024);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{{\"text\":\"{padding}\"}}}}\n"
        );
        let mut writer = peer.take_writer().expect("写侧可用");
        let sender = tokio::spawn(async move {
            let _ = writer.write_all(line.as_bytes()).await;
            let _ = writer.flush().await;
        });
        let notification = tokio::time::timeout(Duration::from_secs(10), conn.next_notification())
            .await
            .expect("1–2MiB 非引用行必须正常解析")
            .expect("应有通知");
        sender.await.ok();
        match notification {
            AdapterNotification::Log(params) => {
                assert_eq!(
                    params["text"].as_str().unwrap_or_default().len(),
                    1536 * 1024
                );
            }
            other => panic!("通知类型不符: {other:?}"),
        }
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
        assert_eq!(conn.invalid_frames_total(), 0);
    }

    #[tokio::test]
    async fn artifact_ref_under_1mib_parses_and_connection_survives() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let padding = "a".repeat(512 * 1024);
        // M2-09 全帧形状（`params` 为 `{session_id?, run_id?, refs}`）；M1-09 时期的
        // 占位形状（`params.pad`）在形状校验落地后按「无效帧」计数（见
        // `artifact_ref_malformed_params_count_as_invalid_frame`）。
        peer.send_line(&format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"artifact_ref\",\"type\":\"artifact_ref\",\"params\":{{\"session_id\":\"01JTEST\",\"refs\":[{{\"path\":\"shot.png\",\"size\":1,\"kind\":\"image/png\"}}],\"pad\":\"{padding}\"}}}}"
        ))
        .await;
        let notification = tokio::time::timeout(Duration::from_secs(5), conn.next_notification())
            .await
            .expect("artifact_ref 引用帧（<1MiB）应正常解析")
            .expect("应有通知");
        match notification {
            AdapterNotification::ArtifactRef(artifact_ref) => {
                assert_eq!(artifact_ref.session_id.as_deref(), Some("01JTEST"));
                assert_eq!(artifact_ref.refs.len(), 1);
                assert_eq!(artifact_ref.refs[0].path, "shot.png");
                assert_eq!(artifact_ref.refs[0].size, 1);
                assert_eq!(artifact_ref.refs[0].kind.as_deref(), Some("image/png"));
                assert!(
                    artifact_ref.refs[0].path.len() < 1024 * 1024,
                    "引用行数据体必须 <1MiB"
                );
            }
            other => panic!("通知类型不符: {other:?}"),
        }
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
        assert_eq!(conn.invalid_frames_total(), 0);
    }

    #[tokio::test]
    async fn artifact_ref_malformed_params_count_as_invalid_frame() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        peer.send_line(
            "{\"jsonrpc\":\"2.0\",\"method\":\"artifact_ref\",\"type\":\"artifact_ref\",\"params\":{\"refs\":\"not-an-array\"}}",
        )
        .await;
        assert!(
            wait_for_count(&conn, 1, Duration::from_secs(5)).await,
            "形状非法的引用帧必须计为无效帧"
        );
        assert!(
            matches!(
                conn.state(),
                ConnectionState::Degraded {
                    invalid_frame_streak: 1,
                    ..
                }
            ),
            "单次无效帧应进入 Degraded（未达 20 阈值不断连）: {:?}",
            conn.state()
        );
        // 真实进程侧「连接仍可用」断言见 m2_09_artifacts 集成测试。
    }

    #[tokio::test]
    async fn artifact_ref_over_1mib_disconnects_with_contract_violation() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let padding = "a".repeat(1200 * 1024);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"artifact_ref\",\"type\":\"artifact_ref\",\"params\":{{\"pad\":\"{padding}\"}}}}\n"
        );
        let mut writer = peer.take_writer().expect("写侧可用");
        let sender = tokio::spawn(async move {
            let _ = writer.write_all(line.as_bytes()).await;
        });
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(5))
            .await
            .expect("1–2MiB 声称 artifact_ref 必须断连");
        sender.abort();
        match reason {
            DisconnectReason::ArtifactRefContractViolation { bytes } => {
                assert!(
                    bytes <= ARTIFACT_REF_LIMIT + 256 * 1024,
                    "报错时缓冲量应受契约上限约束: {bytes}"
                );
            }
            other => panic!("断连原因不符: {other:?}"),
        }
        assert!(
            conn.recorded_errors()
                .iter()
                .any(|line| line.contains("artifact_ref")),
            "必须记录契约违约错误: {:?}",
            conn.recorded_errors()
        );
    }

    #[tokio::test]
    async fn line_over_2mib_disconnects_with_limit_reason() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        let padding = "x".repeat(2560 * 1024);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{{\"text\":\"{padding}\"}}}}\n"
        );
        let mut writer = peer.take_writer().expect("写侧可用");
        let sender = tokio::spawn(async move {
            let _ = writer.write_all(line.as_bytes()).await;
        });
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(5))
            .await
            .expect(">2MiB 行必须断连");
        sender.abort();
        match reason {
            DisconnectReason::LineTooLong { limit } => {
                assert_eq!(limit, crate::framing::MAX_FRAME_BYTES);
            }
            other => panic!("断连原因不符: {other:?}"),
        }
    }

    #[tokio::test]
    async fn half_line_stream_close_reports_incomplete_bytes() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        // 半行：写残行后直接断流（不补 LF，也不经过出站编码器）。
        peer.send_bytes(b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":")
            .await;
        drop(peer);
        let reason = conn
            .wait_for_disconnect(Duration::from_secs(2))
            .await
            .expect("半行断流应断连");
        match reason {
            DisconnectReason::StreamClosed {
                incomplete_line_bytes,
            } => assert!(
                incomplete_line_bytes > 0,
                "必须报告残行字节数（实际 {incomplete_line_bytes}；记录={:?}）",
                conn.recorded_errors()
            ),
            other => panic!("断连原因不符: {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_notification_is_validated_and_forwarded() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": {
                "v": 1,
                "id": "01J00000000000000000000001",
                "session_id": "s-1",
                "run_id": null,
                "runtime_id": "mock",
                "seq": 1,
                "ts": 1,
                "type": "log",
                "payload": {"level": "info", "message": "hi"},
            },
        }))
        .await;
        let notification = tokio::time::timeout(Duration::from_secs(2), conn.next_notification())
            .await
            .unwrap()
            .unwrap();
        match notification {
            AdapterNotification::Event(envelope) => {
                assert_eq!(envelope.seq, 1);
                assert_eq!(envelope.event_type().as_str(), "log");
            }
            other => panic!("通知类型不符: {other:?}"),
        }

        // 信封字段非法（payload 与 type 不匹配）→ 计入无效帧。
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": {
                "v": 1,
                "id": "01J00000000000000000000002",
                "session_id": "s-1",
                "run_id": null,
                "runtime_id": "mock",
                "seq": 2,
                "ts": 1,
                "type": "log",
                "payload": {"level": "nope", "message": "bad"},
            },
        }))
        .await;
        wait_until(|| conn.invalid_frame_streak() == 1).await;
    }

    #[tokio::test]
    async fn unknown_jsonrpc_envelope_members_are_ignored() {
        // D6：JSON-RPC 外层未知成员一律忽略（hello / 通知 / 响应三路径）。
        let (conn, mut peer) = connected(20);
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "hello",
            "future_outer_member": {"x": 1},
            "params": {
                "protocol": "1.0",
                "runtime": {"name": "stub", "version": "0.1.0", "future": true},
            },
        }))
        .await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .expect("hello 外层未知成员必须被忽略");

        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "log",
            "unknown_outer": 42,
            "params": {"level": "info", "message": "ok"},
        }))
        .await;
        let notification = tokio::time::timeout(Duration::from_secs(2), conn.next_notification())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(notification, AdapterNotification::Log(_)));

        let (result, ()) = tokio::join!(conn.request(Method::HealthPing, json!({})), async {
            let request = peer.next_value().await;
            let id = request["id"].as_u64().unwrap();
            peer.send_value(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"status": "ok"},
                "extra_response_member": true,
            }))
            .await;
        });
        assert_eq!(result.unwrap()["status"], "ok");
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
        assert_eq!(conn.invalid_frames_total(), 0, "未知外层成员不得计为无效帧");
    }

    #[tokio::test]
    async fn unknown_notification_method_is_ignored_without_disconnect() {
        let (conn, mut peer) = connected(20);
        peer.hello().await;
        conn.handshake_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
        peer.send_value(json!({
            "jsonrpc": "2.0",
            "method": "future.unknown",
            "params": {"x": 1},
        }))
        .await;
        let notification = tokio::time::timeout(Duration::from_secs(2), conn.next_notification())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            notification,
            AdapterNotification::Other { ref method, .. } if method == "future.unknown"
        ));
        assert!(matches!(conn.state(), ConnectionState::Ready(_)));
    }
}
