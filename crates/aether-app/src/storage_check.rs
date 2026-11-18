//! 启动检查设备/并发/诊断类型（M10-10；ADR-039 v1.0 D-039-3）。
//!
//! 本模块不属于 IPC 响应契约（`scripts/ci/m9-01-baseline.mjs` 的 `RESPONSE_SOURCES`
//! 不包含本文件）；`core_health` 重新导出供测试/证据使用。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sysinfo::{DiskKind, Disks};

/// `health.storage_check` 契约版本（ADR-040 D-040-4：2=ADR-040；缺失/1=ADR-039）。
pub const STORAGE_CHECK_CONTRACT_VERSION: u32 = 2;
/// 运行期周期检查轮询兜底（ADR-040 D-040-3 §5：默认 1 小时；内部常量，不是设置键）。
pub const PERIODIC_CHECK_POLL: Duration = Duration::from_secs(60 * 60);
/// 周期完整检查超时（ADR-040 D-040-3 §5：暂定 30 分钟；按 5.22 GiB P99 证据调整）。
pub const PERIODIC_CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 调度器监督重启退避（ADR-040 D-040-3 §5：1s/5s/30s）。
pub const SCHEDULER_RESTART_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
/// 连续监督失败判定窗口（ADR-040 D-040-3 §5：连续 3 次/5 分钟内无法恢复 → 降级）。
pub const SCHEDULER_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
/// shutdown/app_exit 等待检查收口的 grace（ADR-040 D-040-3 §5）。
pub const SHUTDOWN_CHECK_GRACE: Duration = Duration::from_secs(5);
/// 运行期检查并发上限（ADR-040 D-040-3 §5：`min(startup_profile.concurrency, 2)`）。
pub const RUNTIME_CHECK_MAX_CONCURRENCY: usize = 2;

/// 导入维护窗口的只读来源（`import_control.rs` 持有/复位；调度器只读）。
pub trait ImportActivitySource: Send + Sync + 'static {
    fn import_in_progress(&self) -> bool;
}

/// 备份/恢复维护态的只读来源（`health.maintenance` 同源）。
pub trait MaintenanceActivitySource: Send + Sync + 'static {
    fn maintenance_active(&self) -> bool;
}

impl MaintenanceActivitySource for crate::ipc::MaintenanceCell {
    fn maintenance_active(&self) -> bool {
        self.get().is_some()
    }
}

/// `import_control.rs` 的导入活动标志（ADR-040 D-040-3 §5 / §4.1）。
#[derive(Debug, Default)]
pub struct ImportActivity {
    active: AtomicBool,
}

impl ImportActivity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn clear(&self) {
        self.set(false);
    }
}

impl ImportActivitySource for ImportActivity {
    fn import_in_progress(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// 便捷句柄（`Arc` 共享；`ImportControlBackend` 与调度器共用一个实例）。
pub type ImportActivityHandle = Arc<ImportActivity>;

/// 设备类型（ADR-039 v1.0 D-039-3：NVMe 8 / SATA SSD 4 / HDD 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StorageDeviceClass {
    Hdd,
    Ssd,
    Nvme,
    Unknown,
}

impl StorageDeviceClass {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Hdd => "hdd",
            Self::Ssd => "ssd",
            Self::Nvme => "nvme",
            Self::Unknown => "unknown",
        }
    }

    /// 启动检查并发（ADR-039 D-039-3；未知按默认 4）。
    pub const fn startup_check_concurrency(self) -> usize {
        match self {
            Self::Hdd => 2,
            Self::Ssd => 4,
            Self::Nvme => 8,
            Self::Unknown => DEFAULT_STARTUP_CHECK_CONCURRENCY,
        }
    }
}

/// 启动检查并发决策（证据/诊断；不进入 IPC 契约）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StartupCheckProfile {
    pub device: StorageDeviceClass,
    pub concurrency: usize,
    /// `auto` / `env:device` / `env:concurrency`。
    pub source: &'static str,
}

/// M10-10 启动检查默认并发（ADR-039 D-039-3：默认 4）。
pub const DEFAULT_STARTUP_CHECK_CONCURRENCY: usize = 4;
/// 兼容旧常量名（默认并发；实际并发按设备类型/环境覆盖）。
pub const STARTUP_CHECK_CONCURRENCY: usize = DEFAULT_STARTUP_CHECK_CONCURRENCY;

/// 解析设备类型（`AETHER_STORAGE_DEVICE=hdd|ssd|nvme|unknown` 可覆盖；测试/运维用）。
fn device_class_override() -> Option<StorageDeviceClass> {
    match std::env::var("AETHER_STORAGE_DEVICE")
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "hdd" => Some(StorageDeviceClass::Hdd),
        "ssd" => Some(StorageDeviceClass::Ssd),
        "nvme" => Some(StorageDeviceClass::Nvme),
        "unknown" => Some(StorageDeviceClass::Unknown),
        _ => None,
    }
}

/// 检测数据目录所在设备的类型。
///
/// - Windows/macOS/Linux：`sysinfo::DiskKind::{HDD, SSD}` 判定 HDD/SSD；
/// - Linux：块设备名以 `nvme` 开头时判定 NVMe；
/// - Windows 无安全 FFI 读取 BusType 的路径，NVMe 与 SATA SSD 均按 SSD（4）处理；
///   需要 8 并发时使用 `AETHER_STORAGE_DEVICE=nvme`（证据/运维覆盖）。
pub fn detect_storage_device(data_dir: &Path) -> StorageDeviceClass {
    if let Some(device) = device_class_override() {
        return device;
    }
    let canonical = std::fs::canonicalize(data_dir)
        .map(|path| strip_verbatim_prefix(&path))
        .unwrap_or_else(|_| strip_verbatim_prefix(data_dir));
    let disks = Disks::new_with_refreshed_list();
    let disk = disks
        .list()
        .iter()
        .filter(|disk| canonical.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count());
    let Some(disk) = disk else {
        return StorageDeviceClass::Unknown;
    };
    #[cfg(target_os = "linux")]
    {
        if disk
            .name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("nvme")
        {
            return StorageDeviceClass::Nvme;
        }
    }
    match disk.kind() {
        DiskKind::HDD => StorageDeviceClass::Hdd,
        DiskKind::SSD => StorageDeviceClass::Ssd,
        DiskKind::Unknown(_) => StorageDeviceClass::Unknown,
    }
}

/// Windows `canonicalize` 返回 `\\?\C:\...`（verbatim），需还原为 `C:\...` 才能与
/// `sysinfo` 的挂载点做前缀比较；`\\?\UNC\server\share` → `\\server\share`。
fn strip_verbatim_prefix(path: &Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// 计算启动检查并发（ADR-039 D-039-3：NVMe 8 / SATA SSD 4 / HDD 2；默认 4、最大 8）。
pub fn startup_check_profile(data_dir: &Path) -> StartupCheckProfile {
    if let Ok(raw) = std::env::var("AETHER_STARTUP_CHECK_CONCURRENCY") {
        if let Ok(value) = raw.trim().parse::<usize>() {
            let concurrency = value.clamp(1, 8);
            return StartupCheckProfile {
                device: detect_storage_device(data_dir),
                concurrency,
                source: "env:concurrency",
            };
        }
    }
    let device = detect_storage_device(data_dir);
    StartupCheckProfile {
        device,
        concurrency: device.startup_check_concurrency(),
        source: if device_class_override().is_some() {
            "env:device"
        } else {
            "auto"
        },
    }
}

/// 单项检查结果（证据：单分片/主库耗时与失败数；不进入 IPC 契约）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShardCheckDiagnostic {
    pub shard_id: u8,
    pub ms: f64,
    pub ok: bool,
}

/// 启动/周期检查诊断（`m10_10_startup` 与证据脚本读取）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageCheckDiagnostics {
    pub device: String,
    pub concurrency: usize,
    pub main_ms: Option<f64>,
    pub main_ok: Option<bool>,
    pub shards: Vec<ShardCheckDiagnostic>,
    pub failure_count: usize,
    pub mode: String,
}

impl Default for StorageCheckDiagnostics {
    fn default() -> Self {
        Self {
            device: StorageDeviceClass::Unknown.code().to_owned(),
            concurrency: DEFAULT_STARTUP_CHECK_CONCURRENCY,
            main_ms: None,
            main_ok: None,
            shards: Vec::new(),
            failure_count: 0,
            mode: "startup".to_owned(),
        }
    }
}
