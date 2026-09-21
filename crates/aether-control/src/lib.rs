//! Aether 生命周期与调度层（设计 D2 / D4）。
//!
//! 依赖方向（AGENTS.md §2.1）：依赖 `aether-core` 与 `aether-store`（D4 事件管线的
//! journal 与补读经 D3 存储层），禁止依赖 adapters/security/tauri。
//!
//! M1-05 交付（设计 D4）：
//! - [`normalizer`]：适配器事件 → 事件类型映射 → serde 严格校验（未知字段拒绝、附录 B 白名单）；
//! - [`sequencer`]：会话内单一 sequencer，seq 单调唯一；崩溃后从 `max(seq)+1` 恢复；
//! - [`delta`]：`message.delta` 16ms/8KB 合并；`message.completed` 终稿不受影响；
//! - [`pipeline`]：先日志后广播、补读（10k 上限）、持久化降级状态机、`health` 呈现；
//! - [`journal`] / [`source`]：D3 写队列与读连接池的抽象（生产实现）与故障注入接缝；
//! - [`storage_state`]：`persist_degraded` 进入/退出断言（P0 无热恢复，重启 + 自检恢复）。
//!
//! 硬约束（AGENTS.md §2.2）：禁止 `unwrap()` / `expect()` / `panic!()`
//! （经 workspace clippy lint 强制；测试代码在 crate 级显式豁免）。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backpressure;
pub mod cancel;
pub mod clock;
pub mod delta;
pub mod error;
pub mod journal;
pub mod lifecycle;
pub mod normalizer;
pub mod permission;
pub mod pipeline;
pub mod sequencer;
pub mod source;
pub mod storage_state;
mod time;
mod ulid;

pub use backpressure::{
    BackpressureConfig, BackpressureController, BackpressureError, BackpressureMetrics,
    ControlReadGate, DeliveryPoll, IsolationReason, IsolationSink, NoopIsolationSink,
    PressurePhase, DELIVERY_MAX_BYTES, DELIVERY_NO_FALL_MS, DELIVERY_PER_SESSION_ITEMS,
    DELIVERY_POLL_BATCH, DELIVERY_RESTART_DELAY_MS, PAUSE_BUDGET_MS, PAUSE_BUDGET_WINDOW_MS,
    PAUSE_MAX_CONSECUTIVE_TIMEOUTS, PAUSE_TIMEOUT_MS, PAUSE_WINDOW_MS, RELEASE_SUSTAIN_MS,
    STORAGE_L1_THRESHOLD, STORAGE_L2_THRESHOLD,
};
pub use cancel::{
    CancelTree, RunCancelToken, TaskDump, TaskWatchdog, TASK_DUMP_ACTION_FORCED_CLEANUP,
    TASK_DUMP_CAPACITY, TASK_FORCE_CLEANUP_MS,
};
pub use clock::{Clock, ManualClock, SharedClock, SystemClock};
pub use delta::{DeltaBuffer, DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
pub use error::{JournalError, NormalizeError, PipelineError, SourceError};
pub use journal::{
    JournalFuture, JournalMetrics, JournalReceipt, JournalWriter, PressureLevel, StoreJournal,
};
pub use lifecycle::{
    run_is_retryable, ExecutorFuture, ExecutorOutcome, InterruptReport, LifecycleConfig,
    LifecycleError, RunExecutor, RunRequest, SendAck, SessionManager, MAX_WAITING_RUNS_PER_SESSION,
    RUN_STREAM_TIMEOUT_CODE, RUN_STREAM_TIMEOUT_MS, WATCHDOG_TICK,
};
pub use normalizer::{Normalizer, PendingEvent};
pub use permission::{
    path_violation_code, PermissionConfig, PermissionError, PermissionRequest,
    PermissionResolution, PermissionService,
};
pub use pipeline::{
    EventPipeline, PipelineConfig, PipelineHealth, ReadbackFrame, RunInterrupt, SubmitOutcome,
    BROADCAST_CAPACITY, DEDUP_CAPACITY, MAX_WRITE_ATTEMPTS, PERSIST_RETRY_DELAY, READBACK_MAX_GAP,
    READBACK_PAGE_SIZE, RUN_INTERRUPT_CAPACITY, RUN_INTERRUPT_REASON_DEGRADED,
    SUBMIT_QUEUE_CAPACITY,
};
pub use sequencer::SessionSequencer;
pub use source::{EventSource, SourceFuture, StoreEventSource};
pub use storage_state::{
    DegradeTrigger, StartupSelfCheckReport, StorageState, StorageStateMachine,
    SPACE_GUARD_MIN_FREE_BYTES,
};

/// 控制层版本号——取自单一版本来源（工作区 `Cargo.toml`）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_depends_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
