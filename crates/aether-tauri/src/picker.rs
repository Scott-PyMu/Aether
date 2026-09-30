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

/// 目录/文件选择器：返回所选路径；`Ok(None)` 表示用户取消。
///
/// M3-09（ADR-010 决策 1）在目录选择之上扩展文件选择（`ref_pick` 的 `kind=file`）：
/// - `pick_file` 带默认实现（不可用），仅实现目录选择的旧替身无需改动；
/// - 生产实现 [`TauriDialogPicker`] 两者皆备；E2E/测试替身 [`FixedDirectoryPicker`]
///   分别为目录/文件注入脚本化响应。
pub trait DirectoryPicker: Send + Sync + 'static {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PickError>;

    /// 文件选择（M3-09 `ref_pick({ kind: "file" })`）：默认不可用。
    fn pick_file(&self) -> Result<Option<PathBuf>, PickError> {
        Err(PickError::unavailable(
            "文件选择器未接线（仅生产运行形态；测试需注入 DirectoryPicker 替身）",
        ))
    }
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

/// 测试替身：脚本化返回。
///
/// - 队列长度 >1：按 FIFO 依次消费；
/// - 队列长度为 1：始终返回该结果（E2E 单次注入的常规形态）；
/// - 队列为空：等价取消（`Ok(None)`）。
#[derive(Default)]
pub struct FixedDirectoryPicker {
    responses: Mutex<VecDeque<Result<Option<PathBuf>, PickError>>>,
    /// 文件选择响应队列（M3-09 `ref_pick`；与目录队列独立）。
    file_responses: Mutex<VecDeque<Result<Option<PathBuf>, PickError>>>,
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

    /// 固定返回所选文件（M3-09 文件选择队列）。
    pub fn with_file_path(path: PathBuf) -> Self {
        Self::new().then_file_path(path)
    }

    /// 固定返回文件选择取消。
    pub fn with_file_cancel() -> Self {
        Self::new().then_file_cancel()
    }

    /// 固定返回文件选择错误。
    pub fn with_file_error(message: impl Into<String>) -> Self {
        Self::new().then_file_error(message)
    }

    pub fn then_file_path(mut self, path: PathBuf) -> Self {
        self.push_file(Ok(Some(path)));
        self
    }

    pub fn then_file_cancel(mut self) -> Self {
        self.push_file(Ok(None));
        self
    }

    pub fn then_file_error(mut self, message: impl Into<String>) -> Self {
        self.push_file(Err(PickError::failed(message)));
        self
    }

    fn push(&mut self, response: Result<Option<PathBuf>, PickError>) {
        match self.responses.get_mut() {
            Ok(queue) => queue.push_back(response),
            Err(poisoned) => poisoned.into_inner().push_back(response),
        }
    }

    fn push_file(&mut self, response: Result<Option<PathBuf>, PickError>) {
        match self.file_responses.get_mut() {
            Ok(queue) => queue.push_back(response),
            Err(poisoned) => poisoned.into_inner().push_back(response),
        }
    }
}

/// 消费脚本化队列：长度 >1 按 FIFO；长度 1 恒定返回；空等价取消。
fn consume(
    responses: &Mutex<VecDeque<Result<Option<PathBuf>, PickError>>>,
) -> Result<Option<PathBuf>, PickError> {
    let mut queue = match responses.lock() {
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

impl DirectoryPicker for FixedDirectoryPicker {
    fn pick_directory(&self) -> Result<Option<PathBuf>, PickError> {
        consume(&self.responses)
    }

    fn pick_file(&self) -> Result<Option<PathBuf>, PickError> {
        consume(&self.file_responses)
    }
}
