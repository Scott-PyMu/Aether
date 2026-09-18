//! D5 资源监控：`sysinfo` **5s 采样**；RSS >1GB 或 CPU >200% **持续 60s** → 告警事件
//! （**不自动杀**，D5 硬口径）。
//!
//! - [`ResourceMonitor`]：持续超限判定（时间注入，单测可精确复现）；
//! - [`SysinfoSampler`]：读取目标 PID 的 RSS/CPU 快照；
//! - 告警出口：`SupervisorObserver::on_resource_alert`。

use std::sync::Mutex;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, System};

use super::state::ResourceLimitKind;

/// D5 采样间隔（5s）。
pub const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// D5 RSS 上限（1GB）。
pub const RESOURCE_RSS_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
/// D5 CPU 上限（200%）。
pub const RESOURCE_CPU_LIMIT_PERCENT: f32 = 200.0;
/// D5 持续时长（60s）。
pub const RESOURCE_BREACH_SUSTAIN: Duration = Duration::from_secs(60);

/// 采样快照。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    pub rss_bytes: u64,
    pub cpu_percent: f32,
}

impl ResourceSample {
    /// 是否触达任一下限（超限即开始计时）。
    pub fn exceeded(&self) -> Option<ResourceLimitKind> {
        if self.rss_bytes > RESOURCE_RSS_LIMIT_BYTES {
            return Some(ResourceLimitKind::Rss);
        }
        if self.cpu_percent > RESOURCE_CPU_LIMIT_PERCENT {
            return Some(ResourceLimitKind::Cpu);
        }
        None
    }
}

/// 持续超限判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceObservation {
    pub limit: ResourceLimitKind,
    /// 累计持续时长（毫秒）。
    pub sustained_ms: u64,
}

/// 持续超限监视器（同一段超限只告警一次；回落即复位）。
#[derive(Debug, Default)]
pub struct ResourceMonitor {
    breach_since_ms: Option<u64>,
    reported: bool,
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一次采样；达到「持续 60s」时返回告警（每次超限区间至多一次）。
    pub fn observe(&mut self, now_ms: u64, sample: ResourceSample) -> Option<ResourceObservation> {
        match sample.exceeded() {
            Some(limit) => {
                if self.reported {
                    return None;
                }
                let since = *self.breach_since_ms.get_or_insert(now_ms);
                let sustained_ms = now_ms.saturating_sub(since);
                if sustained_ms >= RESOURCE_BREACH_SUSTAIN.as_millis() as u64 {
                    self.reported = true;
                    return Some(ResourceObservation {
                        limit,
                        sustained_ms,
                    });
                }
                None
            }
            None => {
                self.breach_since_ms = None;
                self.reported = false;
                None
            }
        }
    }

    /// 当前是否处于超限区间（诊断/断言用）。
    pub fn breach_since_ms(&self) -> Option<u64> {
        self.breach_since_ms
    }
}

/// 基于 `sysinfo` 的 PID 采样器。
#[derive(Debug)]
pub struct SysinfoSampler {
    system: Mutex<System>,
    refresh: ProcessRefreshKind,
}

impl SysinfoSampler {
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
            refresh: ProcessRefreshKind::new().with_memory().with_cpu(),
        }
    }

    /// 采样一次；进程不存在时返回 `None`。
    pub fn sample(&self, pid: u32) -> Option<ResourceSample> {
        let mut system = match self.system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        system.refresh_processes_specifics(self.refresh);
        system
            .process(Pid::from_u32(pid))
            .map(|process| ResourceSample {
                rss_bytes: process.memory(),
                cpu_percent: process.cpu_usage(),
            })
    }
}

impl Default for SysinfoSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rss_bytes: u64, cpu_percent: f32) -> ResourceSample {
        ResourceSample {
            rss_bytes,
            cpu_percent,
        }
    }

    #[test]
    fn constants_match_d5() {
        assert_eq!(RESOURCE_SAMPLE_INTERVAL, Duration::from_secs(5));
        assert_eq!(RESOURCE_RSS_LIMIT_BYTES, 1024 * 1024 * 1024);
        assert_eq!(RESOURCE_CPU_LIMIT_PERCENT, 200.0);
        assert_eq!(RESOURCE_BREACH_SUSTAIN, Duration::from_secs(60));
    }

    #[test]
    fn rss_alert_only_after_sustained_60s() {
        let mut monitor = ResourceMonitor::new();
        let over = sample(RESOURCE_RSS_LIMIT_BYTES + 1, 0.0);
        assert_eq!(monitor.observe(0, over), None);
        assert_eq!(monitor.observe(30_000, over), None);
        assert_eq!(
            monitor.observe(60_000, over),
            Some(ResourceObservation {
                limit: ResourceLimitKind::Rss,
                sustained_ms: 60_000,
            })
        );
        // 同一超限区间只告警一次。
        assert_eq!(monitor.observe(90_000, over), None);
    }

    #[test]
    fn cpu_alert_after_sustained_60s() {
        let mut monitor = ResourceMonitor::new();
        let over = sample(1024, RESOURCE_CPU_LIMIT_PERCENT + 0.5);
        assert_eq!(monitor.observe(0, over), None);
        assert_eq!(
            monitor.observe(61_000, over),
            Some(ResourceObservation {
                limit: ResourceLimitKind::Cpu,
                sustained_ms: 61_000,
            })
        );
    }

    #[test]
    fn breach_resets_when_usage_falls_back() {
        let mut monitor = ResourceMonitor::new();
        let over = sample(RESOURCE_RSS_LIMIT_BYTES + 1, 0.0);
        let normal = sample(1024, 1.0);
        assert_eq!(monitor.observe(0, over), None);
        assert_eq!(monitor.observe(30_000, normal), None);
        assert_eq!(monitor.breach_since_ms(), None);
        // 重新超限后需要重新累计 60s。
        assert_eq!(monitor.observe(40_000, over), None);
        assert_eq!(monitor.observe(90_000, over), None);
        assert!(monitor.observe(100_001, over).is_some());
    }

    #[test]
    fn exact_limit_is_not_exceeded() {
        let monitor = sample(RESOURCE_RSS_LIMIT_BYTES, RESOURCE_CPU_LIMIT_PERCENT);
        assert_eq!(monitor.exceeded(), None);
        assert_eq!(
            sample(RESOURCE_RSS_LIMIT_BYTES + 1, 0.0).exceeded(),
            Some(ResourceLimitKind::Rss)
        );
        assert_eq!(
            sample(0, RESOURCE_CPU_LIMIT_PERCENT + 0.1).exceeded(),
            Some(ResourceLimitKind::Cpu)
        );
    }

    #[test]
    fn sampler_reads_own_process() {
        let sampler = SysinfoSampler::new();
        let pid = std::process::id();
        let sample = sampler.sample(pid);
        assert!(sample.is_some(), "应能读取自身进程 RSS/CPU");
        assert!(sampler.sample(u32::MAX).is_none(), "不存在的 PID 返回 None");
    }
}
