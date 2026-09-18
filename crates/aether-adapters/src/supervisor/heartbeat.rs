//! D5 心跳（ADR-003）：每 **10s** `health.ping`（超时 5s），**连续 3 次失败** → 重启。
//!
//! 计时口径（D5）：
//! - 计时起点 = 最近一次成功响应或适配器 Ready 时刻；
//! - 失败采样 = 发起点计时、响应超时/错误即记一次失败；
//! - 本模块只做计数判定（纯逻辑），实际 `health.ping` 由监督器执行。

use std::time::Duration;

/// D5 心跳间隔（10s）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// D6 方法表：`health.ping` 超时 5s。
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
/// D5：连续 3 次失败 → 重启。
pub const HEARTBEAT_MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// 心跳参数（默认严格取 D5；测试可压缩以缩短集成耗时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatConfig {
    pub interval: Duration,
    pub timeout: Duration,
    pub max_consecutive_failures: u32,
}

impl HeartbeatConfig {
    /// D5 默认值（生产固定）。
    pub const fn d5() -> Self {
        Self {
            interval: HEARTBEAT_INTERVAL,
            timeout: HEARTBEAT_TIMEOUT,
            max_consecutive_failures: HEARTBEAT_MAX_CONSECUTIVE_FAILURES,
        }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self::d5()
    }
}

/// 单次心跳采样结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatVerdict {
    /// 仍健康（失败计数未达阈值）。
    Healthy,
    /// 连续失败达到阈值 → 触发重启（D5）。
    Unhealthy { consecutive_failures: u32 },
}

/// 心跳失败计数器。
#[derive(Debug, Clone)]
pub struct HeartbeatMonitor {
    config: HeartbeatConfig,
    consecutive_failures: u32,
    total_success: u64,
    total_failure: u64,
    last_success_ms: Option<u64>,
}

impl HeartbeatMonitor {
    pub fn new(config: HeartbeatConfig) -> Self {
        Self {
            config,
            consecutive_failures: 0,
            total_success: 0,
            total_failure: 0,
            last_success_ms: None,
        }
    }

    pub fn config(&self) -> HeartbeatConfig {
        self.config
    }

    /// 收到成功响应：清零连续失败计数，刷新计时起点。
    pub fn record_success(&mut self, at_ms: u64) {
        self.consecutive_failures = 0;
        self.total_success = self.total_success.saturating_add(1);
        self.last_success_ms = Some(at_ms);
    }

    /// 记录一次失败（超时或错误）。
    pub fn record_failure(&mut self, _at_ms: u64) -> HeartbeatVerdict {
        self.total_failure = self.total_failure.saturating_add(1);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= self.config.max_consecutive_failures {
            HeartbeatVerdict::Unhealthy {
                consecutive_failures: self.consecutive_failures,
            }
        } else {
            HeartbeatVerdict::Healthy
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn total_success(&self) -> u64 {
        self.total_success
    }

    pub fn total_failure(&self) -> u64 {
        self.total_failure
    }

    pub fn last_success_ms(&self) -> Option<u64> {
        self.last_success_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_d5() {
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(10));
        assert_eq!(HEARTBEAT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(HEARTBEAT_MAX_CONSECUTIVE_FAILURES, 3);
        assert_eq!(HeartbeatConfig::default(), HeartbeatConfig::d5());
    }

    #[test]
    fn three_consecutive_failures_trigger_restart() {
        let mut monitor = HeartbeatMonitor::new(HeartbeatConfig::d5());
        assert_eq!(monitor.record_failure(0), HeartbeatVerdict::Healthy);
        assert_eq!(monitor.consecutive_failures(), 1);
        assert_eq!(monitor.record_failure(1), HeartbeatVerdict::Healthy);
        assert_eq!(monitor.consecutive_failures(), 2);
        assert_eq!(
            monitor.record_failure(2),
            HeartbeatVerdict::Unhealthy {
                consecutive_failures: 3
            }
        );
    }

    #[test]
    fn success_resets_failure_streak_and_marks_start_point() {
        let mut monitor = HeartbeatMonitor::new(HeartbeatConfig::d5());
        let _ = monitor.record_failure(10);
        let _ = monitor.record_failure(20);
        monitor.record_success(30);
        assert_eq!(monitor.consecutive_failures(), 0);
        assert_eq!(monitor.last_success_ms(), Some(30));
        // 再失败两次仍健康（计数从 0 重新开始）。
        assert_eq!(monitor.record_failure(40), HeartbeatVerdict::Healthy);
        assert_eq!(monitor.record_failure(50), HeartbeatVerdict::Healthy);
        assert_eq!(
            monitor.record_failure(60),
            HeartbeatVerdict::Unhealthy {
                consecutive_failures: 3
            }
        );
        assert_eq!(monitor.total_success(), 1);
        assert_eq!(monitor.total_failure(), 5);
    }

    #[test]
    fn custom_config_threshold_respected() {
        let config = HeartbeatConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(50),
            max_consecutive_failures: 2,
        };
        let mut monitor = HeartbeatMonitor::new(config);
        assert_eq!(monitor.record_failure(0), HeartbeatVerdict::Healthy);
        assert_eq!(
            monitor.record_failure(1),
            HeartbeatVerdict::Unhealthy {
                consecutive_failures: 2
            }
        );
    }
}
