//! D5 资源监控：`sysinfo` **5s 采样**；RSS >1GB 或 CPU >200% **持续 60s** → 告警事件
//! （**不自动杀**，D5 硬口径）。
//!
//! - [`ResourceMonitor`]：持续超限判定（时间注入，单测可精确复现）；
//! - [`SysinfoSampler`]：读取目标 PID 的 RSS/CPU 快照；
//! - 告警出口：`SupervisorObserver::on_resource_alert`；
//! - **测试阈值钩子**（M1-10 增量，常量级调参）：显式设置下列环境变量时覆盖阈值，
//!   未设置/非法值一律回落到 D5 默认（生产默认路径不依赖 env）：
//!   `AETHER_TEST_RSS_THRESHOLD_MB`（默认 1024）、`AETHER_TEST_CPU_THRESHOLD_PCT`
//!   （默认 200）、`AETHER_TEST_SUSTAIN_SECS`（默认 60）。采样周期固定 5s，不受 env 影响。

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

/// 测试阈值钩子：RSS 上限（MiB；默认 1024）。
pub const ENV_RSS_THRESHOLD_MB: &str = "AETHER_TEST_RSS_THRESHOLD_MB";
/// 测试阈值钩子：CPU 上限（百分比；默认 200）。
pub const ENV_CPU_THRESHOLD_PCT: &str = "AETHER_TEST_CPU_THRESHOLD_PCT";
/// 测试阈值钩子：持续时长（秒；默认 60）。
pub const ENV_SUSTAIN_SECS: &str = "AETHER_TEST_SUSTAIN_SECS";

/// 资源阈值配置（默认严格等于 D5；仅测试经环境变量覆盖）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceConfig {
    /// RSS 上限（字节）。
    pub rss_limit_bytes: u64,
    /// CPU 上限（百分比）。
    pub cpu_limit_percent: f32,
    /// 超限持续时长。
    pub breach_sustain: Duration,
}

impl ResourceConfig {
    /// D5 默认：RSS 1GB / CPU 200% / 持续 60s。
    pub const fn d5() -> Self {
        Self {
            rss_limit_bytes: RESOURCE_RSS_LIMIT_BYTES,
            cpu_limit_percent: RESOURCE_CPU_LIMIT_PERCENT,
            breach_sustain: RESOURCE_BREACH_SUSTAIN,
        }
    }

    /// 读取环境变量覆盖；未设置或非法值回落到 D5 默认。
    ///
    /// 该钩子仅用于集成测试/故障注入（M4-01 复用）；生产不设置这些变量时
    /// 行为与 D5 常量完全一致。
    pub fn from_env() -> Self {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    /// 便于单测的注入式读取（避免修改进程环境）。
    pub fn from_env_with<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let defaults = Self::d5();
        let rss_limit_bytes = lookup(ENV_RSS_THRESHOLD_MB)
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .map(|mb| mb.saturating_mul(1024 * 1024))
            .unwrap_or(defaults.rss_limit_bytes);
        let cpu_limit_percent = lookup(ENV_CPU_THRESHOLD_PCT)
            .and_then(|raw| raw.trim().parse::<f32>().ok())
            .filter(|pct| pct.is_finite() && *pct > 0.0)
            .unwrap_or(defaults.cpu_limit_percent);
        let breach_sustain = lookup(ENV_SUSTAIN_SECS)
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(defaults.breach_sustain);
        Self {
            rss_limit_bytes,
            cpu_limit_percent,
            breach_sustain,
        }
    }

    /// 按本配置判定采样是否超限（RSS 优先于 CPU，与 D5 单测口径一致）。
    pub fn sample_exceeded(&self, sample: &ResourceSample) -> Option<ResourceLimitKind> {
        if sample.rss_bytes > self.rss_limit_bytes {
            return Some(ResourceLimitKind::Rss);
        }
        if sample.cpu_percent > self.cpu_limit_percent {
            return Some(ResourceLimitKind::Cpu);
        }
        None
    }
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self::d5()
    }
}

/// 采样快照。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    pub rss_bytes: u64,
    pub cpu_percent: f32,
}

impl ResourceSample {
    /// 是否触达任一下限（D5 默认阈值；超限即开始计时）。
    pub fn exceeded(&self) -> Option<ResourceLimitKind> {
        self.exceeded_with(ResourceConfig::d5())
    }

    /// 按给定阈值判定（测试钩子/故障注入用）。
    pub fn exceeded_with(&self, config: ResourceConfig) -> Option<ResourceLimitKind> {
        config.sample_exceeded(self)
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
    config: ResourceConfig,
    breach_since_ms: Option<u64>,
    reported: bool,
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 使用自定义阈值（集成测试经 [`ResourceConfig::from_env`] 注入）。
    pub fn with_config(config: ResourceConfig) -> Self {
        Self {
            config,
            breach_since_ms: None,
            reported: false,
        }
    }

    pub fn config(&self) -> ResourceConfig {
        self.config
    }

    /// 喂入一次采样；达到「持续 `breach_sustain`」时返回告警（每次超限区间至多一次）。
    pub fn observe(&mut self, now_ms: u64, sample: ResourceSample) -> Option<ResourceObservation> {
        match self.config.sample_exceeded(&sample) {
            Some(limit) => {
                if self.reported {
                    return None;
                }
                let since = *self.breach_since_ms.get_or_insert(now_ms);
                let sustained_ms = now_ms.saturating_sub(since);
                if sustained_ms >= self.config.breach_sustain.as_millis() as u64 {
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

    #[test]
    fn env_hook_names_are_frozen() {
        assert_eq!(ENV_RSS_THRESHOLD_MB, "AETHER_TEST_RSS_THRESHOLD_MB");
        assert_eq!(ENV_CPU_THRESHOLD_PCT, "AETHER_TEST_CPU_THRESHOLD_PCT");
        assert_eq!(ENV_SUSTAIN_SECS, "AETHER_TEST_SUSTAIN_SECS");
        // 未设置 env 时读取结果与 D5 默认完全一致（生产默认路径不依赖 env）。
        assert_eq!(
            ResourceConfig::from_env_with(|_| None),
            ResourceConfig::d5()
        );
    }

    #[test]
    fn env_hook_overrides_thresholds() {
        let lookup = |key: &str| match key {
            ENV_RSS_THRESHOLD_MB => Some("50".to_owned()),
            ENV_CPU_THRESHOLD_PCT => Some("120.5".to_owned()),
            ENV_SUSTAIN_SECS => Some("2".to_owned()),
            _ => None,
        };
        let config = ResourceConfig::from_env_with(lookup);
        assert_eq!(config.rss_limit_bytes, 50 * 1024 * 1024);
        assert_eq!(config.cpu_limit_percent, 120.5);
        assert_eq!(config.breach_sustain, Duration::from_secs(2));
    }

    #[test]
    fn env_hook_rejects_invalid_values_and_falls_back_to_d5() {
        let cases: Vec<(&str, &str)> = vec![
            (ENV_RSS_THRESHOLD_MB, "abc"),
            (ENV_RSS_THRESHOLD_MB, "0"),
            (ENV_CPU_THRESHOLD_PCT, "NaN"),
            (ENV_CPU_THRESHOLD_PCT, "-1"),
            (ENV_SUSTAIN_SECS, "0"),
            (ENV_SUSTAIN_SECS, "1.5"),
        ];
        for (key, value) in cases {
            let config =
                ResourceConfig::from_env_with(|probe| (probe == key).then(|| value.to_owned()));
            assert_eq!(config, ResourceConfig::d5(), "{key}={value} 应回落 D5");
        }
    }

    #[test]
    fn custom_config_shortens_sustain_and_lowers_rss_threshold() {
        let config = ResourceConfig {
            rss_limit_bytes: 50 * 1024 * 1024,
            cpu_limit_percent: ResourceConfig::d5().cpu_limit_percent,
            breach_sustain: Duration::from_secs(1),
        };
        let mut monitor = ResourceMonitor::with_config(config);
        assert_eq!(monitor.config(), config);
        let over = sample(60 * 1024 * 1024, 0.0);
        assert_eq!(monitor.observe(0, over), None);
        assert_eq!(monitor.observe(999, over), None);
        assert_eq!(
            monitor.observe(1_000, over),
            Some(ResourceObservation {
                limit: ResourceLimitKind::Rss,
                sustained_ms: 1_000,
            })
        );
        // 未覆盖阈值的 D5 监视器在同一采样下不告警（60MB < 1GB）。
        let mut default_monitor = ResourceMonitor::new();
        assert_eq!(default_monitor.observe(0, over), None);
        assert_eq!(default_monitor.observe(60_000, over), None);
    }
}
