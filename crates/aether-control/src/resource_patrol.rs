//! 核心进程 RSS 巡检（M2-07 DoD3；设计 D2 缓解措施：2GB 告警、2.5GB 强制 delta 限流）。
//!
//! 职责：
//! - **采样**：每 [`ResourcePatrolConfig::interval`]（D2 默认 5s）读取核心进程 RSS；
//!   采样源经 [`RssSampler`] 注入（生产 [`SysinfoRssSampler`]，测试用固定值替身）；
//! - **判定**：RSS ≥ [`RSS_ALERT_BYTES`]（2GB）→ [`ResourcePressure::Alert`]；
//!   RSS ≥ [`RSS_THROTTLE_BYTES`]（2.5GB）→ [`ResourcePressure::Throttled`]；
//!   低于阈值时回落（重新越限可再次告警；同一段越限不重复告警）；
//! - **上报**：[`EventPipeline::report_resource_pressure`]——升级产生 `error` 事件
//!   （`core_rss_alert` / `core_rss_throttle`；先日志后广播），`Throttled` 同时放宽
//!   delta 合并窗口（[`crate::pipeline::RSS_THROTTLE_DELTA_INTERVAL`]）；
//! - **参数化注入**：测试/演练经 `AETHER_TEST_RSS_ALERT_MB` / `AETHER_TEST_RSS_THROTTLE_MB` /
//!   `AETHER_TEST_RSS_INTERVAL_MS` 覆盖阈值与周期（仅显式设置且合法时生效；未设置时
//!   严格等于 D2 常量）。
//!
//! P0 只告警与限流，**不**自动杀进程（与 M1-10 适配器资源监视同口径）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, System};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::pipeline::{EventPipeline, ResourcePressure, RSS_ALERT_BYTES, RSS_THROTTLE_BYTES};

/// RSS 巡检周期（D2：5s 采样，与 M1-10 适配器资源监视同频）。
pub const RESOURCE_PATROL_INTERVAL: Duration = Duration::from_secs(5);
/// 告警阈值覆盖（MiB；测试/演练注入，仅 >0 且 < 限流阈值时生效）。
pub const ENV_RSS_ALERT_MB: &str = "AETHER_TEST_RSS_ALERT_MB";
/// 限流阈值覆盖（MiB）。
pub const ENV_RSS_THROTTLE_MB: &str = "AETHER_TEST_RSS_THROTTLE_MB";
/// 巡检周期覆盖（毫秒；>0 时生效）。
pub const ENV_PATROL_INTERVAL_MS: &str = "AETHER_TEST_RSS_INTERVAL_MS";

/// RSS 采样源（生产 sysinfo / 测试替身）。
pub trait RssSampler: Send + Sync + 'static {
    /// 当前进程 RSS（字节）；不可用 → `None`（本次跳过，不改变压力等级）。
    fn sample_rss_bytes(&self) -> Option<u64>;
}

/// 基于 `sysinfo` 的核心进程采样器（与 aether-adapters 同源实现）。
#[derive(Debug)]
pub struct SysinfoRssSampler {
    system: Mutex<System>,
    refresh: ProcessRefreshKind,
    pid: Pid,
}

impl SysinfoRssSampler {
    /// 采样当前进程。
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
            refresh: ProcessRefreshKind::new().with_memory(),
            pid: Pid::from_u32(std::process::id()),
        }
    }
}

impl Default for SysinfoRssSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl RssSampler for SysinfoRssSampler {
    fn sample_rss_bytes(&self) -> Option<u64> {
        let mut system = match self.system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        system.refresh_processes_specifics(self.refresh);
        system.process(self.pid).map(|process| process.memory())
    }
}

/// RSS 巡检配置（D2 默认值；故障注入经 [`ResourcePatrolConfig::from_env`] 覆盖）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePatrolConfig {
    /// 采样周期。
    pub interval: Duration,
    /// 告警阈值（≥ 触发 `Alert`）。
    pub alert_bytes: u64,
    /// 强制 delta 限流阈值（≥ 触发 `Throttled`）。
    pub throttle_bytes: u64,
}

impl ResourcePatrolConfig {
    /// D2 默认：5s / 2GB / 2.5GB。
    pub const fn d2() -> Self {
        Self {
            interval: RESOURCE_PATROL_INTERVAL,
            alert_bytes: RSS_ALERT_BYTES,
            throttle_bytes: RSS_THROTTLE_BYTES,
        }
    }

    /// 环境变量覆盖（仅用于测试/演练；非法值逐项回退 D2 常量）。
    ///
    /// 兼容性约束：`alert` 必须 < `throttle`，否则两项一起回退（避免出现
    /// 「先限流后告警」的不可达状态）。
    pub fn from_env() -> Self {
        let base = Self::d2();
        let interval = positive_env_ms(ENV_PATROL_INTERVAL_MS)
            .map(Duration::from_millis)
            .unwrap_or(base.interval);
        let alert = positive_env_mib(ENV_RSS_ALERT_MB).unwrap_or(base.alert_bytes);
        let throttle = positive_env_mib(ENV_RSS_THROTTLE_MB).unwrap_or(base.throttle_bytes);
        if alert == 0 || throttle == 0 || alert >= throttle {
            return Self { interval, ..base };
        }
        Self {
            interval,
            alert_bytes: alert,
            throttle_bytes: throttle,
        }
    }
}

impl Default for ResourcePatrolConfig {
    fn default() -> Self {
        Self::d2()
    }
}

fn positive_env_ms(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let value = raw.trim().parse::<u64>().ok()?;
    (value > 0).then_some(value)
}

fn positive_env_mib(name: &str) -> Option<u64> {
    positive_env_ms(name).and_then(|mib| mib.checked_mul(1024 * 1024))
}

/// 巡检诊断快照（测试/诊断包消费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourcePatrolSnapshot {
    /// 当前压力等级。
    pub pressure: ResourcePressure,
    /// 最近一次采样值（字节；未采样 → `None`）。
    pub last_rss_bytes: Option<u64>,
    /// 采样次数（含不可用采样）。
    pub samples: u64,
    /// 升级为 `Alert` 的次数（告警事件产出过的次数）。
    pub alert_events: u64,
    /// 升级为 `Throttled` 的次数（限流事件产出过的次数）。
    pub throttle_events: u64,
}

#[derive(Debug, Default)]
struct PatrolState {
    pressure: ResourcePressure,
    last_rss_bytes: Option<u64>,
    samples: u64,
    alert_events: u64,
    throttle_events: u64,
}

/// 核心 RSS 巡检器（克隆共享状态；可手动驱动 [`ResourcePatrol::patrol_once`] 以便断言）。
#[derive(Clone)]
pub struct ResourcePatrol {
    config: Arc<ResourcePatrolConfig>,
    sampler: Arc<dyn RssSampler>,
    state: Arc<Mutex<PatrolState>>,
}

impl ResourcePatrol {
    /// 组装巡检器（不启动后台任务）。
    pub fn new(config: ResourcePatrolConfig, sampler: Arc<dyn RssSampler>) -> Self {
        Self {
            config: Arc::new(config),
            sampler,
            state: Arc::new(Mutex::new(PatrolState::default())),
        }
    }

    /// 生产默认（sysinfo 采样 + D2 阈值；env 覆盖供测试/演练）。
    pub fn with_env() -> Self {
        Self::new(
            ResourcePatrolConfig::from_env(),
            Arc::new(SysinfoRssSampler::new()),
        )
    }

    /// 巡检配置。
    pub fn config(&self) -> &ResourcePatrolConfig {
        &self.config
    }

    /// 诊断快照。
    pub fn snapshot(&self) -> ResourcePatrolSnapshot {
        let state = lock_state(&self.state);
        ResourcePatrolSnapshot {
            pressure: state.pressure,
            last_rss_bytes: state.last_rss_bytes,
            samples: state.samples,
            alert_events: state.alert_events,
            throttle_events: state.throttle_events,
        }
    }

    /// 采样并判定一次；越限升级时经管线上报（返回变化后的压力等级）。
    ///
    /// - 同一段越限只上报一次（升级语义）；
    /// - 回落仅更新等级（不产生事件；再次越限可重新告警）；
    /// - 采样不可用 → `None`（不改变等级）；等级未变化 → `None`。
    pub async fn patrol_once(&self, pipeline: &EventPipeline) -> Option<ResourcePressure> {
        let sample = self.sampler.sample_rss_bytes();
        let previous = {
            let mut state = lock_state(&self.state);
            state.samples += 1;
            if let Some(sample) = sample {
                state.last_rss_bytes = Some(sample);
            }
            state.pressure
        };
        let sample = sample?;
        let target = if sample >= self.config.throttle_bytes {
            ResourcePressure::Throttled
        } else if sample >= self.config.alert_bytes {
            ResourcePressure::Alert
        } else {
            ResourcePressure::Normal
        };
        {
            let mut state = lock_state(&self.state);
            state.pressure = target;
        }
        if target == previous {
            return None;
        }
        let emitted = pipeline
            .report_resource_pressure(target, sample)
            .await
            .unwrap_or(false);
        if emitted {
            let mut state = lock_state(&self.state);
            match target {
                ResourcePressure::Alert => state.alert_events += 1,
                ResourcePressure::Throttled => state.throttle_events += 1,
                ResourcePressure::Normal => {}
            }
        }
        Some(target)
    }

    /// 启动后台巡检（每 `interval` 采样一次；核心运行期长驻任务，D2）。
    pub fn start(self, pipeline: EventPipeline, handle: &Handle) -> JoinHandle<()> {
        handle.spawn(async move {
            loop {
                tokio::time::sleep(self.config.interval).await;
                let _ = self.patrol_once(&pipeline).await;
            }
        })
    }
}

fn lock_state(mutex: &Arc<Mutex<PatrolState>>) -> std::sync::MutexGuard<'_, PatrolState> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d2_config_matches_design_thresholds() {
        let config = ResourcePatrolConfig::d2();
        assert_eq!(config.interval, Duration::from_secs(5));
        assert_eq!(config.alert_bytes, 2 * 1024 * 1024 * 1024, "D2：2GB 告警");
        assert_eq!(
            config.throttle_bytes,
            5 * 512 * 1024 * 1024,
            "D2：2.5GB 强制 delta 限流"
        );
        assert!(config.alert_bytes < config.throttle_bytes);
    }

    #[test]
    fn env_hook_names_are_frozen() {
        assert_eq!(ENV_RSS_ALERT_MB, "AETHER_TEST_RSS_ALERT_MB");
        assert_eq!(ENV_RSS_THROTTLE_MB, "AETHER_TEST_RSS_THROTTLE_MB");
        assert_eq!(ENV_PATROL_INTERVAL_MS, "AETHER_TEST_RSS_INTERVAL_MS");
    }

    #[test]
    fn sampler_reads_current_process_rss() {
        let sampler = SysinfoRssSampler::new();
        let rss = sampler.sample_rss_bytes().expect("当前进程必须可采样");
        assert!(rss > 0, "RSS 必须为正：{rss}");
    }
}
