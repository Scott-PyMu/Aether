//! 安全级别探针（M3-05；设计 D10/A3、UI-UX S-05）。
//!
//! `settings-security-level` 的只读数据源：启动时经 OS 凭据库执行 A3 自检
//! （写入 → 读取 → 删除测试项，[`aether_security::self_check`]），结果随启动快照
//! 暴露给设置页；诊断包导出时同口径复采一次。
//!
//! 边界：P0 无密钥消费路径，本探针只判定**安全级别**（OS 凭据库 / 降级），
//! 不挂载 A3 加密文件、不索取口令（`SecurityManager::initialize` 的完整降级路径
//! 随密钥功能在 P1 接线，登记于 M3-05 证据）。

use serde::Serialize;

/// 安全级别：OS 凭据库（A3 正常路径）。
pub const SECURITY_LEVEL_OS: &str = "os";
/// 安全级别：降级（OS 凭据库不可用；P0 未挂载加密文件）。
pub const SECURITY_LEVEL_DEGRADED: &str = "degraded";

/// 安全级别快照（启动快照 `security_level` 与诊断包 `security` 段共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecurityLevelView {
    /// `os` / `degraded`（UI-UX S-05 `data-level` 取值）。
    pub level: String,
    /// 面向诊断的说明（不含密钥；探针值运行期生成且已删除）。
    pub detail: String,
}

/// 安全级别探针（测试/演练可注入固定值）。
pub trait SecurityProbe: Send + Sync + 'static {
    fn status(&self) -> SecurityLevelView;
}

/// 生产探针：A3 自检（写 → 读 → 删）经 OS 凭据库；失败 → `degraded`。
pub struct NativeSecurityProbe;

impl SecurityProbe for NativeSecurityProbe {
    fn status(&self) -> SecurityLevelView {
        match aether_security::self_check(&aether_security::KeyringStore::new()) {
            Ok(()) => SecurityLevelView {
                level: SECURITY_LEVEL_OS.to_owned(),
                detail: "OS 凭据库自检通过（写入→读取→删除）".to_owned(),
            },
            Err(error) => SecurityLevelView {
                level: SECURITY_LEVEL_DEGRADED.to_owned(),
                detail: format!("OS 凭据库不可用：{error}"),
            },
        }
    }
}

/// 启动探针（生产入口；`run()` 在启动序列早期调用一次）。
pub fn probe() -> SecurityLevelView {
    NativeSecurityProbe.status()
}
