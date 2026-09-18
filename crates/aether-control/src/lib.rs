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

pub mod delta;
pub mod error;
pub mod journal;
pub mod normalizer;
pub mod pipeline;
pub mod sequencer;
pub mod source;
pub mod storage_state;
mod time;
mod ulid;

pub use delta::{DeltaBuffer, DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
pub use error::{JournalError, NormalizeError, PipelineError, SourceError};
pub use journal::{
    JournalFuture, JournalMetrics, JournalReceipt, JournalWriter, PressureLevel, StoreJournal,
};
pub use normalizer::{Normalizer, PendingEvent};
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
