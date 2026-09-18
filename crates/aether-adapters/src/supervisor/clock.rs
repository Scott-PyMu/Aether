//! 可注入时钟（退避/熔断、资源持续超限判定用）。
//!
//! 生产路径使用 [`SystemTimeSource`]；单测使用 [`ManualTime`] 精确驱动时间，
//! 避免 `sleep` 造成的慢测试（DoD②：退避 1/2/4/8/16/30s 与 60s 熔断可确定复现）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 毫秒级单调时钟抽象（返回自进程启动起的毫秒数）。
pub trait TimeSource: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> u64;
}

/// 生产时钟：`Instant` 起点，单调不回拨。
#[derive(Debug)]
pub struct SystemTimeSource {
    start: std::time::Instant,
}

impl SystemTimeSource {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for SystemTimeSource {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// 手动时钟（测试用）：显式推进。
#[derive(Debug, Default)]
pub struct ManualTime {
    now_ms: AtomicU64,
}

impl ManualTime {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(start_ms),
        }
    }

    pub fn advance(&self, delta_ms: u64) {
        self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }

    pub fn set(&self, now_ms: u64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }
}

impl TimeSource for ManualTime {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

/// 共享时钟句柄。
pub type SharedClock = Arc<dyn TimeSource>;

pub fn system_clock() -> SharedClock {
    Arc::new(SystemTimeSource::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_time_is_monotonic_on_advance() {
        let clock = ManualTime::new(100);
        assert_eq!(clock.now_ms(), 100);
        clock.advance(50);
        assert_eq!(clock.now_ms(), 150);
        clock.set(1_000);
        assert_eq!(clock.now_ms(), 1_000);
    }

    #[test]
    fn system_clock_monotonic() {
        let clock = SystemTimeSource::new();
        let first = clock.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(clock.now_ms() >= first);
    }
}
