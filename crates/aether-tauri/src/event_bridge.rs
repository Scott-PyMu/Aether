//! `aether://event` 事件桥（M3-01；D7 单通道 / D8 下游永不反压 reader）。
//!
//! 口径：
//! - 单通道 [`crate::bindings::EVENT_CHANNEL`]；信封含 `session_id`，UI 侧过滤（D7）；
//! - 桥接消费 [`aether_control::EventPipeline::subscribe`] 的 `broadcast(4096)`；
//!   慢消费者 `Lagged(k)` 只计诊断并继续消费——**不阻塞管线、不反压 reader**（D8）；
//!   UI 侧依据 `seq` 缺口走补读（EventStore，M3-01 DoD2）；
//! - 事件出口抽象为 [`EventSink`]：生产为 Tauri `emit`，测试以替身注入
//!   （慢消费注入见 `tests/m3_01_event_bridge.rs`）；
//! - 桥接只读转发：不落库、不产生新事件、不修改管线状态。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aether_control::EventPipeline;
use aether_core::EventEnvelope;
use tokio::sync::broadcast;

use crate::bindings::AetherEvent;

/// 事件出口（生产 = Tauri `emit`；测试 = 记录 / 慢速替身）。
pub trait EventSink: Send + Sync + 'static {
    /// 投递一条事件；`Err` 表示该条投递失败（计入 `failed`，不影响后续事件）。
    fn emit(&self, event: &AetherEvent) -> Result<(), String>;
}

/// 桥接诊断计数（不落库；经 `tracing` 输出）。
#[derive(Debug, Default)]
pub struct BridgeMetrics {
    /// 成功转发条数。
    pub forwarded: AtomicU64,
    /// 广播丢弃条数（`Lagged(k)` 累计；UI 侧按 `seq` 缺口补读）。
    pub lagged: AtomicU64,
    /// 转发失败条数（序列化/`emit` 失败）。
    pub failed: AtomicU64,
}

impl BridgeMetrics {
    /// 快照（诊断/测试断言用）。
    pub fn snapshot(&self) -> BridgeMetricsSnapshot {
        BridgeMetricsSnapshot {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            lagged: self.lagged.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

/// [`BridgeMetrics`] 的只读快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BridgeMetricsSnapshot {
    pub forwarded: u64,
    pub lagged: u64,
    pub failed: u64,
}

/// 转发循环（runtime 任务与测试共用）。
///
/// 退出条件：广播通道关闭（[`broadcast::error::RecvError::Closed`]）。
pub async fn forward(
    mut receiver: broadcast::Receiver<EventEnvelope>,
    sink: Arc<dyn EventSink>,
    metrics: Arc<BridgeMetrics>,
) {
    loop {
        match receiver.recv().await {
            Ok(envelope) => match AetherEvent::from_envelope(&envelope) {
                Ok(event) => {
                    if sink.emit(&event).is_ok() {
                        metrics.forwarded.fetch_add(1, Ordering::Relaxed);
                    } else {
                        metrics.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    metrics.failed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %error, "事件桥：信封序列化失败（仅诊断，不落库）");
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // D8：慢消费者只记诊断；不等待、不反压。UI 侧据 seq 缺口补读。
                metrics.lagged.fetch_add(skipped, Ordering::Relaxed);
                tracing::warn!(skipped, "事件桥：广播落后，等待 UI 侧按 last_seq 补读");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// 生产接线：订阅管线并在 Tauri 运行时后台转发（任务随应用生命周期存活）。
///
/// 返回诊断计数句柄（测试/诊断使用）。
pub fn spawn_app<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    pipeline: &EventPipeline,
) -> Arc<BridgeMetrics> {
    let metrics = Arc::new(BridgeMetrics::default());
    let receiver = pipeline.subscribe();
    let task_metrics = Arc::clone(&metrics);
    let _task =
        tauri::async_runtime::spawn(forward(receiver, tauri_sink(app.clone()), task_metrics));
    metrics
}

/// 构造 Tauri `emit` 出口（生产 `emit` 经 `aether://event` 单通道；测试冒烟复用）。
pub fn tauri_sink<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Arc<dyn EventSink> {
    Arc::new(TauriEventSink { app })
}

/// 生产出口：经 Tauri 事件通道向全部 WebView 广播（UI 侧按 `session_id` 过滤）。
struct TauriEventSink<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> EventSink for TauriEventSink<R> {
    fn emit(&self, event: &AetherEvent) -> Result<(), String> {
        use tauri::Emitter;
        self.app
            .emit(crate::bindings::EVENT_CHANNEL, event.clone())
            .map_err(|error| error.to_string())
    }
}
