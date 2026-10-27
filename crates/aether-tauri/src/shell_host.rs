//! 壳层宿主实现（ADR-029 D-029-2）：把 `app_exit` / `app_restart` 映射到 Tauri 生命周期 API。
//!
//! 应用层只依赖 [`aether_app::ShellHost`] trait；本文件是迁移期唯一的 Tauri 实现
//! （M9-07 起由 Electron main 的等价实现替代）。

use aether_app::ShellHost;

/// Tauri `AppHandle` 上的壳层宿主。
pub struct TauriShellHost {
    app: tauri::AppHandle,
}

impl TauriShellHost {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl ShellHost for TauriShellHost {
    fn exit(&self, code: i32) {
        self.app.exit(code);
    }

    fn request_restart(&self) {
        self.app.request_restart();
    }
}
