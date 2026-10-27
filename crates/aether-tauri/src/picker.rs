//! 壳层目录选择器（Tauri dialog）+ 应用层选择器契约再导出。
//!
//! ADR-029 D-029-2：`DirectoryPicker` trait 与测试替身属应用层
//! （`aether-app::picker`）；生产实现（系统对话框）属壳层，见 [`TauriDialogPicker`]。
//! 选择器会阻塞当前线程，调用方必须放到阻塞线程池执行（命令层经 `spawn_blocking`）。

use std::path::PathBuf;

pub use aether_app::picker::{DirectoryPicker, FixedDirectoryPicker, PickError, PickErrorKind};

/// 生产实现：Tauri dialog（`blocking_pick_folder`）。
pub struct TauriDialogPicker {
    app: tauri::AppHandle,
}

impl TauriDialogPicker {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl DirectoryPicker for TauriDialogPicker {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PickError> {
        use tauri_plugin_dialog::DialogExt;
        let picked = self
            .app
            .dialog()
            .file()
            .set_title("选择 Aether 数据目录（本地磁盘）")
            .blocking_pick_folder();
        Ok(picked.and_then(|file| file.into_path().ok()))
    }

    /// M3-09 `ref_pick({ kind: "file" })`：系统文件选择器；路径原样返回
    /// （canonicalize 与可访问性检查在 `artifact_add`，ADR-010 决策 1）。
    fn pick_file(&self) -> Result<Option<PathBuf>, PickError> {
        use tauri_plugin_dialog::DialogExt;
        let picked = self
            .app
            .dialog()
            .file()
            .set_title("选择要引用的文件（只读引用）")
            .blocking_pick_file();
        Ok(picked.and_then(|file| file.into_path().ok()))
    }
}
