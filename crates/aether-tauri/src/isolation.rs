//! `IsolationSink` 生产桥接（M2-04 DoD⑤）：控制层背压熔断 → M1-10 监督器。
//!
//! 契约（[`aether_control::IsolationSink`]）：
//! - `isolate`：`RuntimeSupervisor::isolate(StorageBackpressure, detail)` → `degraded +
//!   status_reason=storage_backpressure` + 进程终止（停止事件生产，D8）；
//! - `release`：`RuntimeSupervisor::release()` → `degraded → starting → ready`（写队列回落
//!   ≤1024 持续 30s 后由控制层触发，ADR-004）。
//!
//! 同步/异步桥接与 `runtime_control.rs` 同口径：future spawn 到核心 tokio 运行时并以
//! `std::sync::mpsc` 等待（不阻塞运行时线程），超时返回 `false`（控制层按 30s 重试）。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::protocol::DisabledReason;
use aether_adapters::supervisor::{IsolationOutcome, ReleaseOutcome, Supervisor};
use aether_control::{IsolationReason, IsolationSink};
use aether_core::RuntimeId;

/// 隔离动作硬超时（终止序列 ≤15s + 重启/握手 ≤10s；见 D5 重启预算）。
pub const ISOLATION_TIMEOUT: Duration = Duration::from_secs(30);

/// 生产实现：M1-10 监督器 + tokio 句柄桥接。
pub struct SupervisorIsolationSink {
    supervisor: Arc<Supervisor>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
}

impl SupervisorIsolationSink {
    pub fn new(
        supervisor: Arc<Supervisor>,
        handle: tokio::runtime::Handle,
        timeout: Duration,
    ) -> Self {
        Self {
            supervisor,
            handle,
            timeout,
        }
    }

    /// spawn future 到核心运行时并等待结果（超时返回 `None`）。
    fn call<F>(&self, future: F) -> Option<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(self.timeout).ok()
    }
}

impl IsolationSink for SupervisorIsolationSink {
    fn isolate(&self, runtime_id: &RuntimeId, reason: IsolationReason, detail: &str) -> bool {
        let supervisor = Arc::clone(&self.supervisor);
        let id = runtime_id.as_str().to_owned();
        let detail = format!("[{}] {detail}", reason.detail_code());
        self.call(async move {
            let Some(runtime) = supervisor.get(&id) else {
                return false;
            };
            matches!(
                runtime
                    .isolate(DisabledReason::StorageBackpressure, &detail)
                    .await,
                IsolationOutcome::Isolated { .. }
            )
        })
        .unwrap_or(false)
    }

    fn release(&self, runtime_id: &RuntimeId) -> bool {
        let supervisor = Arc::clone(&self.supervisor);
        let id = runtime_id.as_str().to_owned();
        self.call(async move {
            let Some(runtime) = supervisor.get(&id) else {
                // 未注册的 runtime 视为无需解除（不阻塞控制层解除路径）。
                return true;
            };
            matches!(
                runtime.release().await,
                ReleaseOutcome::Released | ReleaseOutcome::NotApplicable { .. }
            )
        })
        .unwrap_or(false)
    }
}
