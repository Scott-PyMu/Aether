//! 目录选择器抽象（M1-06 DoD3 迁移主路径的可测试性）。
//!
//! 生产实现 [`TauriDialogPicker`] 调用 Tauri dialog（系统目录选择器，阻塞 API）；
//! 测试替身 [`FixedDirectoryPicker`] 返回预设路径 / 取消 / 错误，使迁移主路径可被
//! 集成测试与 E2E 驱动（E2E 经 debug 探针注入固定路径，见 `startup_probe`）。
//!
//! 分层说明：选择器属桌面壳能力（系统对话框），定义在 `aether-tauri`；`aether-core`
//! 保持纯模型、不依赖 Tauri（AGENTS §2.1）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

/// 目录选择失败分类（稳定契约，前端按 `code` 分支；`message` 仅展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PickErrorKind {
    /// 选择器在当前运行形态不可用（如未接线/无桌面会话）。
    Unavailable,
    /// 选择器调用失败（系统对话框错误等）。
    Failed,
}

/// 目录选择错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickError {
    pub kind: PickErrorKind,
    pub message: String,
}

impl PickError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: PickErrorKind::Unavailable,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: PickErrorKind::Failed,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}：{}", self.kind, self.message)
    }
}

impl std::error::Error for PickError {}

/// 目录选择器：返回所选目录；`Ok(None)` 表示用户取消。
pub trait DirectoryPicker: Send + Sync + 'static {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PickError>;
}

/// 生产实现：Tauri dialog（`blocking_pick_folder`）。
///
/// 该调用会阻塞当前线程，调用方必须放到阻塞线程池（`spawn_blocking`）执行，
/// 不得占用主线程。
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
}

/// 测试替身：脚本化返回。
///
/// - 队列长度 >1：按 FIFO 依次消费；
/// - 队列长度为 1：始终返回该结果（E2E 单次注入的常规形态）；
/// - 队列为空：等价取消（`Ok(None)`）。
#[derive(Default)]
pub struct FixedDirectoryPicker {
    responses: Mutex<VecDeque<Result<Option<PathBuf>, PickError>>>,
}

impl FixedDirectoryPicker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 固定返回所选目录。
    pub fn with_path(path: PathBuf) -> Self {
        Self::new().then_path(path)
    }

    /// 固定返回取消。
    pub fn with_cancel() -> Self {
        Self::new().then_cancel()
    }

    /// 固定返回错误。
    pub fn with_error(message: impl Into<String>) -> Self {
        Self::new().then_error(message)
    }

    pub fn then_path(mut self, path: PathBuf) -> Self {
        self.push(Ok(Some(path)));
        self
    }

    pub fn then_cancel(mut self) -> Self {
        self.push(Ok(None));
        self
    }

    pub fn then_error(mut self, message: impl Into<String>) -> Self {
        self.push(Err(PickError::failed(message)));
        self
    }

    fn push(&mut self, response: Result<Option<PathBuf>, PickError>) {
        match self.responses.get_mut() {
            Ok(queue) => queue.push_back(response),
            Err(poisoned) => poisoned.into_inner().push_back(response),
        }
    }
}

impl DirectoryPicker for FixedDirectoryPicker {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PickError> {
        let mut queue = match self.responses.lock() {
            Ok(queue) => queue,
            Err(poisoned) => poisoned.into_inner(),
        };
        if queue.len() > 1 {
            if let Some(response) = queue.pop_front() {
                return response;
            }
        }
        match queue.front() {
            Some(response) => response.clone(),
            None => Ok(None),
        }
    }
}
