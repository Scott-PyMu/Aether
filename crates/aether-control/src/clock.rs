//! 可注入时钟（M2-01 断流超时 / M2-03 审批超时；批准口径见各任务 DoD「时钟注入」）。
//!
//! 生产使用 [`SystemClock`]（Unix epoch 毫秒）；测试使用 [`ManualClock`] 显式推进，
//! 避免真实等待 120s/300s。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// 时钟抽象（只读 `now_ms`；单调性由实现保证）。
pub trait Clock: Send + Sync + 'static {
    /// 当前时间（Unix epoch 毫秒）。
    fn now_ms(&self) -> i64;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::time::now_ms()
    }
}

/// 手动时钟（测试注入；`advance` 单调推进）。
#[derive(Debug)]
pub struct ManualClock {
    now_ms: AtomicI64,
}

impl ManualClock {
    /// 以起始时间构造。
    pub fn new(start_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(start_ms),
        }
    }

    /// 推进时钟。
    pub fn advance(&self, delta_ms: i64) {
        self.now_ms.fetch_add(delta_ms, Ordering::AcqRel);
    }

    /// 直接设置（须 ≥ 当前值；倒拨返回 `false` 且不修改）。
    pub fn set(&self, now_ms: i64) -> bool {
        let mut current = self.now_ms.load(Ordering::Acquire);
        loop {
            if now_ms < current {
                return false;
            }
            match self.now_ms.compare_exchange_weak(
                current,
                now_ms,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Acquire)
    }
}

/// 共享时钟句柄。
pub type SharedClock = Arc<dyn Clock>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_monotonically() {
        let clock = ManualClock::new(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        clock.advance(500);
        assert_eq!(clock.now_ms(), 1_500);
        assert!(!clock.set(1_000), "禁止倒拨");
        assert_eq!(clock.now_ms(), 1_500);
        assert!(clock.set(2_000));
        assert_eq!(clock.now_ms(), 2_000);
    }

    #[test]
    fn system_clock_is_reasonable_epoch_ms() {
        let now = SystemClock.now_ms();
        assert!(now > 1_577_836_800_000, "应晚于 2020-01-01: {now}");
    }
}
