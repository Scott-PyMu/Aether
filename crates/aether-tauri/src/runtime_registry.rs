//! 壳层资源目录适配 + 应用层运行时注册清单再导出（ADR-029 D-029-2）。
//!
//! 清单解析/校验/拒绝原因全部在 `aether-app::runtime_registry`；本文件只提供
//! 「Tauri 安装产物资源目录」这一壳层信息（ADR-015 安装布局）。

pub use aether_app::runtime_registry::*;

/// 解析安装产物资源目录（Tauri `resource_dir`）。
pub fn resource_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    app.path().resource_dir().ok()
}
