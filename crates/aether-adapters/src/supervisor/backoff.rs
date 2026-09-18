//! D5 重启退避与熔断（评审修订 #4）：
//!
//! - 退避曲线：`1 / 2 / 4 / 8 / 16 / 30s`（第 6 次及以后封顶 30s）；
//! - 熔断：**60s 内 ≥5 次崩溃** → `disabled + status_reason=crash_loop`；
//! - 计数器重置：人工动作（`runtime_retry` / `runtime_enable`）全量重置；
//!   稳定运行 ≥60s 由监督器调用 [`RestartPolicy::mark_stable`] 重置退避级数
//!   （崩溃窗口仍按 60s 滚动裁剪，不丢失熔断判定）。
//!
//! 本模块为纯逻辑（时间经 [`TimeSource`] 注入），单测可精确复现曲线与熔断。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use super::clock::{SharedClock, TimeSource};

/// D5 退避曲线（秒）。
pub const RESTART_BACKOFF_SECONDS: [u64; 6] = [1, 2, 4, 8, 16, 30];

/// 崩溃窗口（D5：60s 内 ≥5 次 → 熔断）。
pub const CRASH_WINDOW: Duration = Duration::from_secs(60);

/// 熔断阈值（崩溃次数）。
pub const CRASH_THRESHOLD: usize = 5;

/// 单次崩溃后的处置决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashDecision {
    /// 按退避曲线等待后重启。
    Backoff(Duration),
    /// 熔断：转入 `disabled + crash_loop`（D5）。
    CircuitBroken {
        /// 窗口内崩溃次数（≥5）。
        crashes_in_window: usize,
    },
}

/// 重启退避/熔断策略（时间经时钟注入，可单测）。
#[derive(Debug)]
pub struct RestartPolicy {
    clock: SharedClock,
    crashes_ms: VecDeque<u64>,
    consecutive: usize,
}

impl RestartPolicy {
    pub fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            crashes_ms: VecDeque::new(),
            consecutive: 0,
        }
    }

    pub fn with_system_clock() -> Self {
        Self::new(Arc::new(super::clock::SystemTimeSource::new()))
    }

    /// 记录一次崩溃；返回本次处置（退避时长或熔断）。
    pub fn record_crash(&mut self) -> CrashDecision {
        let now = self.clock.now_ms();
        self.crashes_ms.push_back(now);
        self.prune(now);
        let crashes_in_window = self.crashes_ms.len();
        if crashes_in_window >= CRASH_THRESHOLD {
            return CrashDecision::CircuitBroken { crashes_in_window };
        }
        let index = self.consecutive.min(RESTART_BACKOFF_SECONDS.len() - 1);
        self.consecutive = index.saturating_add(1);
        CrashDecision::Backoff(Duration::from_secs(RESTART_BACKOFF_SECONDS[index]))
    }

    /// 稳定运行 ≥60s 后重置退避级数（崩溃窗口按滚动裁剪，保持熔断判定）。
    pub fn mark_stable(&mut self) {
        self.consecutive = 0;
    }

    /// 人工动作（`runtime_retry` / `runtime_enable`）全量重置。
    pub fn reset(&mut self) {
        self.crashes_ms.clear();
        self.consecutive = 0;
    }

    /// 当前窗口内崩溃次数（诊断/断言用）。
    pub fn crashes_in_window(&self) -> usize {
        let now = self.clock.now_ms();
        let window = CRASH_WINDOW.as_millis() as u64;
        self.crashes_ms
            .iter()
            .filter(|at| now.saturating_sub(**at) <= window)
            .count()
    }

    /// 当前退避时长（下一次崩溃将使用的值）。
    pub fn current_backoff(&self) -> Duration {
        let index = self.consecutive.min(RESTART_BACKOFF_SECONDS.len() - 1);
        Duration::from_secs(RESTART_BACKOFF_SECONDS[index])
    }

    fn prune(&mut self, now: u64) {
        let window = CRASH_WINDOW.as_millis() as u64;
        while let Some(front) = self.crashes_ms.front() {
            if now.saturating_sub(*front) > window {
                self.crashes_ms.pop_front();
            } else {
                break;
            }
        }
    }
}

/// 时钟访问（便于监督器统一构造）。
pub fn policy_clock(clock: &SharedClock) -> Arc<dyn TimeSource> {
    Arc::clone(clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::clock::ManualTime;

    fn policy(clock: &Arc<ManualTime>) -> RestartPolicy {
        RestartPolicy::new(Arc::clone(clock) as SharedClock)
    }

    #[test]
    fn backoff_curve_is_1_2_4_8_16_30_then_capped() {
        let clock = Arc::new(ManualTime::new(0));
        let mut policy = policy(&clock);
        let expected = [1_u64, 2, 4, 8, 16, 30, 30];
        for (index, seconds) in expected.into_iter().enumerate() {
            let decision = policy.record_crash();
            // 每次崩溃后推进 >60s，避免触发熔断并观察完整曲线。
            clock.advance(61_000);
            assert_eq!(
                decision,
                CrashDecision::Backoff(Duration::from_secs(seconds)),
                "第 {} 次崩溃退避应为 {seconds}s",
                index + 1
            );
        }
    }

    #[test]
    fn five_crashes_within_60s_break_the_circuit() {
        let clock = Arc::new(ManualTime::new(0));
        let mut policy = policy(&clock);
        let mut decisions = Vec::new();
        for index in 0..5 {
            decisions.push(policy.record_crash());
            clock.advance(10_000);
            if index < 4 {
                assert!(matches!(decisions[index], CrashDecision::Backoff(_)));
            }
        }
        assert_eq!(
            decisions[4],
            CrashDecision::CircuitBroken {
                crashes_in_window: 5
            }
        );
    }

    #[test]
    fn crash_window_rolls_off_after_60s() {
        let clock = Arc::new(ManualTime::new(0));
        let mut policy = policy(&clock);
        // 4 次崩溃在 60s 窗口内（退避 1/2/4/8s）。
        for _ in 0..4 {
            let _ = policy.record_crash();
            clock.advance(10_000);
        }
        assert_eq!(policy.crashes_in_window(), 4);
        // 再等待 >60s，窗口内计数归零。
        clock.advance(61_000);
        assert_eq!(policy.crashes_in_window(), 0);
        // 第 5 次崩溃不再熔断（窗口已滚动），退避级数继续上升。
        let decision = policy.record_crash();
        assert!(matches!(decision, CrashDecision::Backoff(_)));
    }

    #[test]
    fn mark_stable_resets_backoff_level_but_keeps_crash_window() {
        let clock = Arc::new(ManualTime::new(0));
        let mut policy = policy(&clock);
        assert_eq!(
            policy.record_crash(),
            CrashDecision::Backoff(Duration::from_secs(1))
        );
        assert_eq!(
            policy.record_crash(),
            CrashDecision::Backoff(Duration::from_secs(2))
        );
        policy.mark_stable();
        assert_eq!(policy.current_backoff(), Duration::from_secs(1));
        assert_eq!(policy.crashes_in_window(), 2);
    }

    #[test]
    fn manual_reset_clears_everything() {
        let clock = Arc::new(ManualTime::new(0));
        let mut policy = policy(&clock);
        for _ in 0..4 {
            let _ = policy.record_crash();
            clock.advance(1_000);
        }
        policy.reset();
        assert_eq!(policy.crashes_in_window(), 0);
        assert_eq!(policy.current_backoff(), Duration::from_secs(1));
    }

    #[test]
    fn constants_match_d5() {
        assert_eq!(RESTART_BACKOFF_SECONDS, [1, 2, 4, 8, 16, 30]);
        assert_eq!(CRASH_WINDOW, Duration::from_secs(60));
        assert_eq!(CRASH_THRESHOLD, 5);
    }
}
