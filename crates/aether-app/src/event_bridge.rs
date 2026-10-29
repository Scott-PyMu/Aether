//! 事件桥（M9-05；ADR-029 D-029-5 / §4.4 / §4.7；设计 D4/D7/D8）。
//!
//! 链路：
//!
//! ```text
//! EventPipeline（先日志后广播）
//!   → 有界 mpsc（4096；daemon 传输 writer）
//!   → EventSink（daemon：stdout JSON-RPC notification `aether.event`）
//!   → Electron main（单通道 FIFO + 16ms 批处理）
//!   → renderer（`aether:event`；EventStore 以 `last_seq` 补读）
//! ```
//!
//! 本模块只负责 **transport-agnostic 的投递语义**；协议帧/通道由壳层实现
//! （[`EventSink`]）。硬约束（AGENTS §2.3、ADR-029 §4.7）：
//!
//! - **先日志后广播**由 [`aether_control::EventPipeline`] 保证：本模块只订阅落盘后的
//!   broadcast，不接触 journal；
//! - **控制事件不丢**：队列满时切 journal 补读（[`EventPipeline::readback`]），
//!   不重启 daemon；`message.delta` / `usage` / `log` 可丢并计数
//!   （`transport_dropped`）；
//! - **连续 3 次补读失败**才升级：重建事件订阅（不是重启进程/连接），写审计
//!   `event.bridge.escalated`；仍不重启 daemon；
//! - `readback_gap_too_large`（缺口 >10k）**不自动重试**：UI 走 `messages_page`
//!   同码提示（D4 失败场景表）；
//! - `seq` 由单一 sequencer 分配；本模块只按 `(session_id, seq)` 去重/保序，
//!   DB `UNIQUE(session_id, seq)` 兜底不变。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::{Clock, EventPipeline, PipelineError, ReadbackFrame, SystemClock};
use aether_core::{EventEnvelope, EventType, SessionId};
use aether_store::{AuditLogRecord, StoreCommand, WriteQueue};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

/// daemon 传输队列容量（ADR-029 §4.7：有界队列，建议 4096）。
pub const DELIVERY_QUEUE_CAPACITY: usize = 4_096;
/// 连续补读失败升级阈值（ADR-029 §4.7：连续 3 次才升级）。
pub const DELIVERY_READBACK_MAX_ATTEMPTS: u32 = 3;
/// 补读失败重试间隔（毫秒级；不改变「连续 3 次」口径）。
pub const DELIVERY_READBACK_RETRY_DELAY: Duration = Duration::from_millis(10);
/// 单次恢复最多读取的页数（10k 缺口 / 500 条页 ≈ 20；留余量）。
pub const DELIVERY_READBACK_MAX_PAGES: usize = 32;
/// 关闭时等待事件桥收敛的上限（daemon 关闭预算内；不阻塞退出）。
pub const DELIVERY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// 升级审计动作名（`audit_log.action`）。
pub const ESCALATION_AUDIT_ACTION: &str = "event.bridge.escalated";
/// 可丢事件类型（ADR-029 §4.7 原文：仅这三类；其余均按控制事件处理）。
pub const DROPPABLE_EVENT_TYPES: [EventType; 3] =
    [EventType::MessageDelta, EventType::Usage, EventType::Log];
/// 事件桥轮询停止信号的周期（无事件时也保证及时退出）。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 事件是否可丢（ADR-029 §4.7）。
pub fn is_droppable(event_type: EventType) -> bool {
    DROPPABLE_EVENT_TYPES.contains(&event_type)
}

/// 投递配置（默认值即 ADR-029 §4.7 口径；测试可注入小队列/短重试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// 有界队列容量（默认 [`DELIVERY_QUEUE_CAPACITY`]）。
    pub queue_capacity: usize,
    /// 连续补读失败升级阈值（默认 [`DELIVERY_READBACK_MAX_ATTEMPTS`]）。
    pub readback_max_attempts: u32,
    /// 补读失败重试间隔（默认 [`DELIVERY_READBACK_RETRY_DELAY`]）。
    pub readback_retry_delay: Duration,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DELIVERY_QUEUE_CAPACITY,
            readback_max_attempts: DELIVERY_READBACK_MAX_ATTEMPTS,
            readback_retry_delay: DELIVERY_READBACK_RETRY_DELAY,
        }
    }
}

impl DeliveryConfig {
    /// 参数校验（容量/阈值必须为正）。
    pub fn validate(&self) -> Result<(), DeliveryConfigError> {
        if self.queue_capacity == 0 {
            return Err(DeliveryConfigError {
                reason: "queue_capacity 必须 >0".to_owned(),
            });
        }
        if self.readback_max_attempts == 0 {
            return Err(DeliveryConfigError {
                reason: "readback_max_attempts 必须 >0".to_owned(),
            });
        }
        Ok(())
    }
}

/// 配置错误（不 panic；daemon 以默认配置回退并记录日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryConfigError {
    pub reason: String,
}

impl std::fmt::Display for DeliveryConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for DeliveryConfigError {}

/// 投递出口（daemon = stdout JSONL writer；测试 = 记录器）。
///
/// 返回 `Err` 表示传输已不可用（stdout 断开）；事件桥停止，不 panic、不重启 daemon。
pub trait EventSink: Send + Sync + 'static {
    fn send(&self, event: &EventEnvelope) -> Result<(), SinkError>;
}

/// 补读异步结果（object-safe；与 `aether-control::SourceFuture` 同口径）。
pub type ReadbackFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReadbackFrame, PipelineError>> + Send + 'a>>;

/// 事件来源（生产 = [`EventPipeline`]；测试/未来 HTTP/WS = 注入实现）。
///
/// 契约与 [`EventPipeline`] 一致：`subscribe` 只返回**落盘后**广播；
/// `readback` 按 `last_seq` 断点续读，缺口 >10k 返回 `readback_gap_too_large`。
pub trait EventFeed: Send + Sync + 'static {
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope>;

    fn readback<'a>(&'a self, session_id: &'a SessionId, last_seq: u64) -> ReadbackFuture<'a>;
}

impl EventFeed for EventPipeline {
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        EventPipeline::subscribe(self)
    }

    fn readback<'a>(&'a self, session_id: &'a SessionId, last_seq: u64) -> ReadbackFuture<'a> {
        Box::pin(EventPipeline::readback(self, session_id, last_seq))
    }
}

/// 出口错误（仅用于日志/诊断，不进入 `aether_code` 命名空间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError {
    pub message: String,
}

impl SinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SinkError {}

/// 投递指标快照（诊断/证据；`transport_dropped` 对应 delta 丢弃计数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeliveryMetricsSnapshot {
    /// 已写入传输出口的事件数。
    pub forwarded: u64,
    /// 队列满丢弃的 delta 类事件数（`transport_dropped`）。
    pub dropped_delta: u64,
    /// 广播 lag 跳过的事件数（随后经 journal 补读恢复）。
    pub lagged_events: u64,
    /// 从 journal 补读的事件数（含重复投递，由 writer 按 seq 去重）。
    pub readback_events: u64,
    /// 补读失败次数（每次失败尝试计 1）。
    pub readback_failures: u64,
    /// 连续补读失败触发的订阅重建次数（不重启 daemon）。
    pub connection_rebuilds: u64,
    /// 传输出口写入失败次数。
    pub sink_errors: u64,
    /// `readback_gap_too_large` 次数（不自动重试；UI 同码提示）。
    pub readback_gap_too_large: u64,
}

#[derive(Debug, Default)]
struct DeliveryMetrics {
    forwarded: AtomicU64,
    dropped_delta: AtomicU64,
    lagged_events: AtomicU64,
    readback_events: AtomicU64,
    readback_failures: AtomicU64,
    connection_rebuilds: AtomicU64,
    sink_errors: AtomicU64,
    readback_gap_too_large: AtomicU64,
}

impl DeliveryMetrics {
    fn snapshot(&self) -> DeliveryMetricsSnapshot {
        DeliveryMetricsSnapshot {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            dropped_delta: self.dropped_delta.load(Ordering::Relaxed),
            lagged_events: self.lagged_events.load(Ordering::Relaxed),
            readback_events: self.readback_events.load(Ordering::Relaxed),
            readback_failures: self.readback_failures.load(Ordering::Relaxed),
            connection_rebuilds: self.connection_rebuilds.load(Ordering::Relaxed),
            sink_errors: self.sink_errors.load(Ordering::Relaxed),
            readback_gap_too_large: self.readback_gap_too_large.load(Ordering::Relaxed),
        }
    }
}

struct SharedState {
    config: DeliveryConfig,
    metrics: Arc<DeliveryMetrics>,
    /// 已成功写入出口的每会话最高 seq（补读断点；writer 单写）。
    last_delivered: Mutex<HashMap<SessionId, u64>>,
    /// 升级审计出口（可选；降级启动无写队列时为 None）。
    audit: Option<WriteQueue>,
}

impl SharedState {
    fn delivered_seq(&self, session_id: &SessionId) -> u64 {
        match self.last_delivered.lock() {
            Ok(guard) => guard.get(session_id).copied().unwrap_or(0),
            Err(poisoned) => poisoned.into_inner().get(session_id).copied().unwrap_or(0),
        }
    }

    fn record_delivered(&self, session_id: SessionId, seq: u64) {
        let mut guard = match self.last_delivered.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = guard.entry(session_id).or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }
}

/// 事件桥句柄（停止 + 指标）。
pub struct DeliveryHandle {
    stop: Option<watch::Sender<bool>>,
    join: JoinHandle<()>,
    metrics: Arc<DeliveryMetrics>,
}

impl DeliveryHandle {
    /// 当前指标快照（关闭前记录证据）。
    pub fn metrics(&self) -> DeliveryMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// 停止事件桥；返回是否在预算内收敛（超时即放弃后台任务，不阻塞退出）。
    pub async fn shutdown(&mut self, timeout: Duration) -> bool {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
        match tokio::time::timeout(timeout, &mut self.join).await {
            Ok(_) => true,
            Err(_) => {
                self.join.abort();
                false
            }
        }
    }
}

impl Drop for DeliveryHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
    }
}

/// 事件桥（构造后经 [`Self::spawn`] 进入核心运行时）。
pub struct EventBridge {
    feed: Arc<dyn EventFeed>,
    sink: Arc<dyn EventSink>,
    config: DeliveryConfig,
    audit: Option<WriteQueue>,
}

impl EventBridge {
    /// 以真实事件管线构造（daemon / Tauri / 未来 HTTP/WS 的默认路径）。
    pub fn new(pipeline: EventPipeline, sink: Arc<dyn EventSink>) -> Self {
        Self::new_with_feed(Arc::new(pipeline), sink)
    }

    /// 以注入事件来源构造（测试；或未来非管线来源）。
    pub fn new_with_feed(feed: Arc<dyn EventFeed>, sink: Arc<dyn EventSink>) -> Self {
        Self {
            feed,
            sink,
            config: DeliveryConfig::default(),
            audit: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: DeliveryConfig) -> Self {
        self.config = config;
        self
    }

    /// 注入审计写队列（连续 3 次补读失败时写 `event.bridge.escalated`）。
    #[must_use]
    pub fn with_audit_write(mut self, write: WriteQueue) -> Self {
        self.audit = Some(write);
        self
    }

    /// 启动事件桥（writer + forwarder 两个任务；返回句柄用于关闭/指标）。
    pub fn spawn(
        self,
        handle: &tokio::runtime::Handle,
    ) -> Result<DeliveryHandle, DeliveryConfigError> {
        self.config.validate()?;
        let (sender, receiver) = mpsc::channel(self.config.queue_capacity);
        let (stop_sender, stop_receiver) = watch::channel(false);
        let state = Arc::new(SharedState {
            config: self.config.clone(),
            metrics: Arc::new(DeliveryMetrics::default()),
            last_delivered: Mutex::new(HashMap::new()),
            audit: self.audit,
        });
        let writer = handle.spawn(run_writer(
            receiver,
            Arc::clone(&self.sink),
            Arc::clone(&state),
        ));
        let receiver = self.feed.subscribe();
        let forwarder = Forwarder {
            feed: self.feed,
            sender,
            state: Arc::clone(&state),
            stop: stop_receiver,
            receiver,
            known_sessions: HashSet::new(),
            consecutive_failures: 0,
        };
        let join = handle.spawn(forwarder.run(writer));
        Ok(DeliveryHandle {
            stop: Some(stop_sender),
            join,
            metrics: Arc::clone(&state.metrics),
        })
    }
}

/// writer：从有界队列取事件 → 传输出口；按 `(session_id, seq)` 去重。
async fn run_writer(
    mut receiver: mpsc::Receiver<EventEnvelope>,
    sink: Arc<dyn EventSink>,
    state: Arc<SharedState>,
) {
    while let Some(event) = receiver.recv().await {
        let session_id = event.session_id.clone();
        let seq = event.seq;
        if seq <= state.delivered_seq(&session_id) {
            // journal 补读与队列中同一事件重复：按 seq 去重（D4 幂等）。
            continue;
        }
        match sink.send(&event) {
            Ok(()) => {
                state.metrics.forwarded.fetch_add(1, Ordering::Relaxed);
                state.record_delivered(session_id, seq);
            }
            Err(error) => {
                state.metrics.sink_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %error, "事件传输出口不可用：事件桥停止（不重启 daemon）");
                break;
            }
        }
    }
}

struct Forwarder {
    feed: Arc<dyn EventFeed>,
    sender: mpsc::Sender<EventEnvelope>,
    state: Arc<SharedState>,
    stop: watch::Receiver<bool>,
    receiver: broadcast::Receiver<EventEnvelope>,
    known_sessions: HashSet<SessionId>,
    consecutive_failures: u32,
}

enum RecoveryOutcome {
    Recovered,
    GapTooLarge,
    Failed { attempts: u32 },
    Closed,
}

impl Forwarder {
    async fn run(mut self, writer: JoinHandle<()>) {
        loop {
            if *self.stop.borrow() {
                break;
            }
            let received = tokio::time::timeout(STOP_POLL_INTERVAL, self.receiver.recv()).await;
            match received {
                Err(_) => continue,
                Ok(Ok(event)) => {
                    if !self.handle_event(event).await {
                        break;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    self.state
                        .metrics
                        .lagged_events
                        .fetch_add(skipped, Ordering::Relaxed);
                    if !self.recover_all_known().await {
                        break;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
            }
        }
        // writer 依赖 sender 关闭退出；给关闭预算（daemon 退出优先）。
        drop(self.sender);
        let _ = tokio::time::timeout(DELIVERY_SHUTDOWN_TIMEOUT, writer).await;
    }

    /// 处理一条落盘后事件；返回 `false` 表示传输已关闭，事件桥停止。
    async fn handle_event(&mut self, event: EventEnvelope) -> bool {
        let session_id = event.session_id.clone();
        self.known_sessions.insert(session_id.clone());
        let event_type = event.event_type();
        match self.sender.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
            Err(mpsc::error::TrySendError::Full(_event)) => {
                if is_droppable(event_type) {
                    // delta 类可丢并计数；UI 以 last_seq 补读（D4/D8）。
                    self.state
                        .metrics
                        .dropped_delta
                        .fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                // 控制事件不得丢：切 journal 补读（不重启 daemon）。
                let outcome = self.recover(&session_id).await;
                self.apply_recovery(&session_id, outcome).await
            }
        }
    }

    async fn recover_all_known(&mut self) -> bool {
        let sessions: Vec<SessionId> = self.known_sessions.iter().cloned().collect();
        for session_id in sessions {
            let outcome = self.recover(&session_id).await;
            if !self.apply_recovery(&session_id, outcome).await {
                return false;
            }
        }
        true
    }

    /// journal 补读：从 `last_delivered` 断点续读，直到追平或失败。
    async fn recover(&mut self, session_id: &SessionId) -> RecoveryOutcome {
        let mut from = self.state.delivered_seq(session_id);
        let mut failures = 0_u32;
        for _ in 0..DELIVERY_READBACK_MAX_PAGES {
            match self.feed.readback(session_id, from).await {
                Ok(frame) => {
                    let next = frame.events.last().map(|event| event.seq);
                    let complete = frame.complete;
                    self.state
                        .metrics
                        .readback_events
                        .fetch_add(frame.events.len() as u64, Ordering::Relaxed);
                    for event in frame.events {
                        if self.sender.send(event).await.is_err() {
                            return RecoveryOutcome::Closed;
                        }
                    }
                    if complete {
                        return RecoveryOutcome::Recovered;
                    }
                    match next {
                        Some(seq) if seq > from => from = seq,
                        _ => return RecoveryOutcome::Recovered,
                    }
                }
                Err(PipelineError::ReadbackGapTooLarge { .. }) => {
                    // 缺口 >10k：拒绝自动补发（UI 以 messages_page 同码提示）。
                    self.state
                        .metrics
                        .readback_gap_too_large
                        .fetch_add(1, Ordering::Relaxed);
                    return RecoveryOutcome::GapTooLarge;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    self.state
                        .metrics
                        .readback_failures
                        .fetch_add(1, Ordering::Relaxed);
                    if failures >= self.state.config.readback_max_attempts {
                        return RecoveryOutcome::Failed { attempts: failures };
                    }
                    tracing::warn!(error = %error, session_id = %session_id, "journal 补读失败：重试");
                    tokio::time::sleep(self.state.config.readback_retry_delay).await;
                }
            }
        }
        self.state
            .metrics
            .readback_failures
            .fetch_add(1, Ordering::Relaxed);
        RecoveryOutcome::Failed { attempts: 1 }
    }

    async fn apply_recovery(&mut self, session_id: &SessionId, outcome: RecoveryOutcome) -> bool {
        match outcome {
            RecoveryOutcome::Recovered | RecoveryOutcome::GapTooLarge => {
                self.consecutive_failures = 0;
                true
            }
            RecoveryOutcome::Closed => false,
            RecoveryOutcome::Failed { attempts } => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(attempts);
                if self.consecutive_failures < self.state.config.readback_max_attempts {
                    return true;
                }
                self.escalate(session_id).await;
                self.consecutive_failures = 0;
                // 重建**事件订阅**（不是 daemon 进程）：继续按新广播补读。
                self.receiver = self.feed.subscribe();
                true
            }
        }
    }

    /// 升级：计数 + 审计 + 重建订阅；不重启 daemon（ADR-029 §4.7）。
    async fn escalate(&self, session_id: &SessionId) {
        self.state
            .metrics
            .connection_rebuilds
            .fetch_add(1, Ordering::Relaxed);
        let last_seq = self.state.delivered_seq(session_id);
        tracing::error!(
            session_id = %session_id,
            last_seq,
            attempts = self.state.config.readback_max_attempts,
            "连续补读失败：重建事件订阅并审计（daemon 不重启）"
        );
        let Some(write) = &self.state.audit else {
            return;
        };
        let detail = serde_json::json!({
            "session_id": session_id.as_str(),
            "last_seq": last_seq,
            "attempts": self.state.config.readback_max_attempts,
            "phase": "subscription_rebuilt",
        });
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: Some(session_id.as_str().to_owned()),
            runtime_id: None,
            actor: "system".to_owned(),
            action: ESCALATION_AUDIT_ACTION.to_owned(),
            resource: Some("aether.event".to_owned()),
            detail: Some(detail.to_string()),
            result: Some("rebuilt".to_owned()),
            ts: SystemClock.now_ms(),
        };
        if let Err(error) = write.execute(StoreCommand::InsertAudit { record }).await {
            tracing::warn!(error = %error, "事件桥升级审计写入失败（继续服务）");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn droppable_set_is_exactly_the_three_registered_types() {
        assert!(is_droppable(EventType::MessageDelta));
        assert!(is_droppable(EventType::Usage));
        assert!(is_droppable(EventType::Log));
        for event_type in EventType::ALL {
            if matches!(
                event_type,
                EventType::MessageDelta | EventType::Usage | EventType::Log
            ) {
                continue;
            }
            assert!(
                !is_droppable(event_type),
                "控制事件不得被归入可丢集合：{}",
                event_type.as_str()
            );
        }
    }

    #[test]
    fn delivery_defaults_are_frozen() {
        let config = DeliveryConfig::default();
        assert_eq!(config.queue_capacity, 4_096);
        assert_eq!(config.readback_max_attempts, 3);
        assert!(config.validate().is_ok());
        assert!(DeliveryConfig {
            queue_capacity: 0,
            ..config.clone()
        }
        .validate()
        .is_err());
        assert!(DeliveryConfig {
            readback_max_attempts: 0,
            ..config
        }
        .validate()
        .is_err());
    }
}
