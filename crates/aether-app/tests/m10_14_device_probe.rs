//! M10-14 设备探测证据：实际 `sysinfo` 分类 + `AETHER_STORAGE_DEVICE` 覆盖。
//!
//! Windows 下 `sysinfo` 只返回 HDD/SSD，不暴露 Storage BusType；NVMe 可能被归为 SSD（4 并发）。
//! 本测试记录实际 profile，并验证显式覆盖（`nvme` → 8）可用；BusType 原始值由
//! `scripts/test/m10-14/device-probe.mjs` 通过 PowerShell `Get-PhysicalDisk` 记录。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use aether_app::core_health::{startup_check_profile, StorageDeviceClass};

#[test]
fn report_auto_profile_and_env_override() {
    let dir = tempfile::tempdir().expect("temp");
    std::env::remove_var("AETHER_STORAGE_DEVICE");
    std::env::remove_var("AETHER_STARTUP_CHECK_CONCURRENCY");

    let auto = startup_check_profile(dir.path());
    println!("M10_14_DEVICE_PROBE auto={auto:?}");
    assert!(
        (1..=8).contains(&auto.concurrency),
        "自动 profile 并发必须在 1..=8：{auto:?}"
    );

    std::env::set_var("AETHER_STORAGE_DEVICE", "nvme");
    let override_profile = startup_check_profile(dir.path());
    println!("M10_14_DEVICE_PROBE override={override_profile:?}");
    assert_eq!(override_profile.device, StorageDeviceClass::Nvme);
    assert_eq!(override_profile.concurrency, 8);
    assert_eq!(override_profile.source, "env:device");
    std::env::remove_var("AETHER_STORAGE_DEVICE");
}
