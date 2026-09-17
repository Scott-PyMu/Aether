//! 单实例锁（M1-06 / T12；设计 D1「插件集保持最小：single-instance」与启动序列第一步）。
//!
//! 行为：第二次启动由 `tauri-plugin-single-instance` 检测到既有实例后，将命令行转发
//! 给首实例并自行退出；首实例回调中聚焦（取消最小化 → 显示 → 置前）已有主窗口。
//! 第二进程在 `setup` 之前退出，不打开数据目录、不建立任何写连接（M1-05 接入存储后
//! 该前置退出语义保持有效）。

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Manager, Runtime};

/// 主窗口标签（与 `tauri.conf.json` 的 `app.windows[0].label` 一致）。
pub const MAIN_WINDOW_LABEL: &str = "main";

/// 单实例插件；必须在 `tauri::Builder` 中第一个注册。
pub fn plugin<R: Runtime>() -> TauriPlugin<R> {
    tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        focus_main_window(app);
        #[cfg(debug_assertions)]
        crate::startup_probe::record_focus();
    })
}

/// 聚焦已有主窗口（幂等）。
pub fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}
