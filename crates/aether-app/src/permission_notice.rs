//! 权限待审批通知桥（M9-06 DoD-5；ADR-029 §4.4 / 设计 D9/D7）。
//!
//! `permission.pending` 是**仅用于即时刷新 UI 的通知**（`{count, session_id?}`）：
//!
//! - 决议路径**只有一条**：`permissions_pending`（轮询/校准）+ `permission_resolve`
//!   （唯一决议入口）；本模块**不**创建第二条决策路径、不消费/回传决议；
//! - 通知由**落盘后**事件（`permission.requested` / `permission.resolved`，
//!   附录 B 现有类型）驱动，计数每次重算（以权限服务当前 pending 台账为准）；
//! - 广播 lag / 计数失败只影响提示新鲜度，不影响正确性（UI 仍以轮询校准）；
//! - 通知本身不改变 `aether.event` 的 9 字段信封，也不新增附录 B 事件类型：
//!   它是 §16.4 登记表内的 daemon → client 通知。
//!
//! 链路（ADR-029 §4.4）：
//!
//! ```text
//! EventPipeline（先日志后广播）
//!   → PermissionNoticeBridge（本模块）
//!   → PermissionNoticeSink（daemon：stdout JSON-RPC notification `permission.pending`）
//!   → Electron main → renderer（UI 刷新提示；决议仍走 permission_resolve）
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aether_control::EventPipeline;
use aether_core::{EventEnvelope, EventType};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::event_bridge::{EventFeed, SinkError};
use crate::ipc::IpcState;

/// `permission.pending` 通知方法名（ADR-029 §16.4 登记表）。
pub const PERMISSION_PENDING_METHOD: &str = "permission.pending";
/// 触发通知重算的附录 B 事件类型（权限生命周期；payload 语义不变）。
pub const PERMISSION_PENDING_EVENT_TYPES: [EventType; 2] = [
    EventType::PermissionRequested,
    EventType::PermissionResolved,
];
/// 无事件时也保证及时响应停止信号。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 通知桥关闭预算（仅提示通道；远小于事件桥 2s，避免挤占 T11 退出预算）。
pub const NOTICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

/// `permission.pending` 通知载荷（ADR-029 §4.4：`{count, session_id?}`）。
///
/// `count` = **当前全局待审批票据数**（badge 用；按权限服务台账现算）；
/// `session_id` = 触发本次通知的会话（UI 可据此只刷新该会话的提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionNotice {
    pub count: usize,
    pub session_id: Option<String>,
}

impl PermissionNotice {
    pub fn new(count: usize, session_id: Option<String>) -> Self {
        Self { count, session_id }
    }

    /// 通知 `params`（恰好 `count` + 可选 `session_id`；不夹带决议字段）。
    pub fn to_params(&self) -> Value {
        match &self.session_id {
            Some(session_id) => json!({ "count": self.count, "session_id": session_id }),
            None => json!({ "count": self.count }),
        }
    }
}

/// 通知出口（daemon = stdout JSON-RPC notification；测试 = 记录器）。
pub trait PermissionNoticeSink: Send + Sync + 'static {
    fn send(&self, notice: &PermissionNotice) -> Result<(), SinkError>;
}

/// 待审批计数来源（生产 = `IpcState::dispatch("permissions_pending")`）。
pub trait PendingCountSource: Send + Sync + 'static {
    fn pending_count(&self) -> Result<usize, String>;
}

/// 生产计数来源：复用 `permissions_pending` 命令（**唯一**待审批事实源）。
pub struct DispatchPendingCount {
    state: IpcState,
}

impl DispatchPendingCount {
    pub fn new(state: IpcState) -> Self {
        Self { state }
    }
}

impl PendingCountSource for DispatchPendingCount {
    fn pending_count(&self) -> Result<usize, String> {
        match self.state.dispatch("permissions_pending", json!({})) {
            Ok(Value::Array(items)) => Ok(items.len()),
            Ok(other) => Err(format!("permissions_pending 返回值不是数组：{other}")),
            Err(error) => Err(format!("{}: {}", error.code.as_str(), error.message)),
        }
    }
}

/// 通知桥指标（诊断/证据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoticeMetricsSnapshot {
    /// 成功写出的通知数。
    pub sent: u64,
    /// 计数查询失败次数（通知仅提示；轮询仍为准）。
    pub count_failures: u64,
    /// 传输出口写入失败次数。
    pub sink_errors: u64,
    /// 广播 lag 跳过的事件数（lag 后立即补发一次全量计数）。
    pub lagged_events: u64,
    /// 广播关闭（管线关闭）导致的退出次数。
    pub closed: u64,
}

#[derive(Debug, Default)]
struct NoticeMetrics {
    sent: AtomicU64,
    count_failures: AtomicU64,
    sink_errors: AtomicU64,
    lagged_events: AtomicU64,
    closed: AtomicU64,
}

impl NoticeMetrics {
    fn snapshot(&self) -> NoticeMetricsSnapshot {
        NoticeMetricsSnapshot {
            sent: self.sent.load(Ordering::Relaxed),
            count_failures: self.count_failures.load(Ordering::Relaxed),
            sink_errors: self.sink_errors.load(Ordering::Relaxed),
            lagged_events: self.lagged_events.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
        }
    }
}

/// 通知桥句柄（停止 + 指标）。
pub struct PermissionNoticeHandle {
    stop: Option<watch::Sender<bool>>,
    join: JoinHandle<()>,
    metrics: Arc<NoticeMetrics>,
}

impl PermissionNoticeHandle {
    pub fn metrics(&self) -> NoticeMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// 停止通知桥；返回是否在预算内收敛（超时即放弃后台任务，不阻塞退出）。
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

impl Drop for PermissionNoticeHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
    }
}

/// 权限待审批通知桥（构造后经 [`Self::spawn`] 进入核心运行时）。
pub struct PermissionNoticeBridge {
    feed: Arc<dyn EventFeed>,
    counter: Arc<dyn PendingCountSource>,
    sink: Arc<dyn PermissionNoticeSink>,
}

impl PermissionNoticeBridge {
    /// 以真实事件管线构造（daemon 默认路径）。
    pub fn new(
        pipeline: EventPipeline,
        counter: Arc<dyn PendingCountSource>,
        sink: Arc<dyn PermissionNoticeSink>,
    ) -> Self {
        Self::new_with_feed(Arc::new(pipeline), counter, sink)
    }

    /// 以注入事件来源构造（测试；或未来非管线来源）。
    pub fn new_with_feed(
        feed: Arc<dyn EventFeed>,
        counter: Arc<dyn PendingCountSource>,
        sink: Arc<dyn PermissionNoticeSink>,
    ) -> Self {
        Self {
            feed,
            counter,
            sink,
        }
    }

    /// 启动通知桥；返回句柄用于关闭/指标。
    pub fn spawn(self, handle: &tokio::runtime::Handle) -> PermissionNoticeHandle {
        let (stop_sender, stop_receiver) = watch::channel(false);
        let metrics = Arc::new(NoticeMetrics::default());
        let receiver = self.feed.subscribe();
        let join = handle.spawn(run_notice_bridge(
            receiver,
            stop_receiver,
            self.counter,
            self.sink,
            Arc::clone(&metrics),
        ));
        PermissionNoticeHandle {
            stop: Some(stop_sender),
            join,
            metrics,
        }
    }
}

fn is_permission_event(event_type: EventType) -> bool {
    PERMISSION_PENDING_EVENT_TYPES.contains(&event_type)
}

async fn run_notice_bridge(
    mut receiver: broadcast::Receiver<EventEnvelope>,
    stop: watch::Receiver<bool>,
    counter: Arc<dyn PendingCountSource>,
    sink: Arc<dyn PermissionNoticeSink>,
    metrics: Arc<NoticeMetrics>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        let received = tokio::time::timeout(STOP_POLL_INTERVAL, receiver.recv()).await;
        match received {
            Err(_) => continue,
            Ok(Ok(event)) => {
                if !is_permission_event(event.event_type()) {
                    continue;
                }
                emit(
                    counter.as_ref(),
                    sink.as_ref(),
                    metrics.as_ref(),
                    Some(event.session_id.as_str().to_owned()),
                );
            }
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                // 提示通道 lag：立即以当前台账补发一次全量计数（无会话过滤）。
                metrics.lagged_events.fetch_add(skipped, Ordering::Relaxed);
                emit(counter.as_ref(), sink.as_ref(), metrics.as_ref(), None);
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                metrics.closed.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

fn emit(
    counter: &dyn PendingCountSource,
    sink: &dyn PermissionNoticeSink,
    metrics: &NoticeMetrics,
    session_id: Option<String>,
) {
    match counter.pending_count() {
        Ok(count) => {
            let notice = PermissionNotice::new(count, session_id);
            match sink.send(&notice) {
                Ok(()) => {
                    metrics.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    metrics.sink_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %error, "permission.pending 通知写入失败（仅提示通道）");
                }
            }
        }
        Err(error) => {
            metrics.count_failures.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                error = %error,
                "permission.pending 计数查询失败（轮询仍为唯一事实源）"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_params_carry_only_registered_fields() {
        let notice = PermissionNotice::new(2, Some("01J00000000000000000000S06".to_owned()));
        let params = notice.to_params();
        let object = params.as_object().expect("对象");
        assert_eq!(object.len(), 2, "只允许 count + session_id");
        assert_eq!(object.get("count"), Some(&json!(2)));
        assert_eq!(
            object.get("session_id"),
            Some(&json!("01J00000000000000000000S06"))
        );
        let global = PermissionNotice::new(0, None).to_params();
        let object = global.as_object().expect("对象");
        assert_eq!(object.len(), 1, "无会话时只带 count");
        assert_eq!(object.get("count"), Some(&json!(0)));
    }

    #[test]
    fn permission_event_set_is_frozen() {
        assert_eq!(PERMISSION_PENDING_EVENT_TYPES.len(), 2);
        assert!(is_permission_event(EventType::PermissionRequested));
        assert!(is_permission_event(EventType::PermissionResolved));
        assert!(!is_permission_event(EventType::MessageDelta));
        assert!(!is_permission_event(EventType::SessionUpdated));
    }
}
