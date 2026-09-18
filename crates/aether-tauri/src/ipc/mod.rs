//! IPC 命令层（设计 D7）：命令面、严格参数校验框架与结构化错误。
//!
//! 安全基线（评审 #7）：
//! - 每个命令先经 [`validate::parse_strict`]（serde `deny_unknown_fields` +
//!   长度 / 枚举 / 格式校验）；
//! - 路径参数经 [`path::validate_user_path`]（canonicalize + 允许根目录前缀比较）；
//! - 校验失败返回 [`IpcError`]（结构化错误码），不调用后端、不落库、不透传下游。

pub mod backend;
pub mod commands;
pub mod dto;
pub mod error;
pub mod path;
pub mod validate;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

pub use backend::IpcBackend;
pub use commands::handler;
pub use error::{IpcError, IpcErrorCode};

use crate::picker::{DirectoryPicker, TauriDialogPicker};
use crate::startup::StartupGate;

/// 命令层共享状态：后端实现 + 路径白名单根目录 + 启动门（M1-06）
/// + 目录选择器（M1-06 迁移主路径可测试性）。
///
/// `backend` 为 `OnceLock`：生产启动在 Builder 阶段先以「延迟注入」构造
/// （窗口加载期状态必须可用——`startup_*` 门命令不依赖后端），
/// 单实例插件初始化之后由 `setup` 打开存储并注入真实后端（ADR-007；
/// T12 语义要求第二实例在 `setup` 前退出、不建写连接）。
pub struct IpcState {
    backend: OnceLock<Arc<dyn IpcBackend>>,
    allowed_roots: Vec<PathBuf>,
    startup: Option<Arc<StartupGate>>,
    picker: OnceLock<Arc<dyn DirectoryPicker>>,
    app: OnceLock<tauri::AppHandle>,
}

impl IpcState {
    /// 未接线启动门的构造（M1-08 校验矩阵与框架测试用；生产见 [`Self::with_startup`]）。
    pub fn new(backend: Arc<dyn IpcBackend>, allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            backend: backend_cell(backend),
            allowed_roots,
            startup: None,
            picker: OnceLock::new(),
            app: OnceLock::new(),
        }
    }

    /// 生产构造：启动门生效后，未 Ready 时业务命令一律 `startup_blocked`。
    pub fn with_startup(
        backend: Arc<dyn IpcBackend>,
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
    ) -> Self {
        Self {
            backend: backend_cell(backend),
            allowed_roots,
            startup: Some(startup),
            picker: OnceLock::new(),
            app: OnceLock::new(),
        }
    }

    /// 生产构造 + 预置目录选择器（E2E 注入固定路径 / 集成测试替身）。
    pub fn with_startup_and_picker(
        backend: Arc<dyn IpcBackend>,
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
        picker: Arc<dyn DirectoryPicker>,
    ) -> Self {
        let state = Self::with_startup(backend, allowed_roots, startup);
        let _ = state.picker.set(picker);
        state
    }

    /// 生产构造（延迟后端）：Builder 阶段先 manage，`setup` 完成存储/管线后
    /// 经 [`Self::install_backend`] 注入（保持窗口加载期 `startup_*` 可用）。
    pub fn with_startup_deferred(allowed_roots: Vec<PathBuf>, startup: Arc<StartupGate>) -> Self {
        Self {
            backend: OnceLock::new(),
            allowed_roots,
            startup: Some(startup),
            picker: OnceLock::new(),
            app: OnceLock::new(),
        }
    }

    /// 延迟构造 + 预置目录选择器（debug/E2E）。
    pub fn with_startup_deferred_and_picker(
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
        picker: Arc<dyn DirectoryPicker>,
    ) -> Self {
        let state = Self::with_startup_deferred(allowed_roots, startup);
        let _ = state.picker.set(picker);
        state
    }

    /// 注入真实后端（仅首次生效；返回 `false` 表示已注入）。
    pub fn install_backend(&self, backend: Arc<dyn IpcBackend>) -> bool {
        self.backend.set(backend).is_ok()
    }

    pub fn startup(&self) -> Option<&StartupGate> {
        self.startup.as_deref()
    }

    pub fn startup_arc(&self) -> Option<Arc<StartupGate>> {
        self.startup.clone()
    }

    /// setup 阶段登记应用句柄（`app_exit` 使用）；未预置选择器时安装真实 Tauri 选择器。
    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        let _ = self
            .picker
            .set(Arc::new(TauriDialogPicker::new(handle.clone())));
        let _ = self.app.set(handle);
    }

    pub(crate) fn app_handle(&self) -> Option<&tauri::AppHandle> {
        self.app.get()
    }

    /// 目录选择器（生产：Tauri dialog；测试/E2E：注入替身）。
    pub(crate) fn picker(&self) -> Option<Arc<dyn DirectoryPicker>> {
        self.picker.get().cloned()
    }

    /// 业务命令入口：启动门未 Ready 时阻断（主界面不可达的命令层兜底）；
    /// 后端尚未注入（启动序列未完成）时返回 `core_not_ready`（ADR-007 增量 2）。
    pub(crate) fn backend_ready(&self) -> Result<&dyn IpcBackend, IpcError> {
        if let Some(gate) = &self.startup {
            gate.ensure_ready()?;
        }
        match self.backend.get() {
            Some(backend) => Ok(&**backend),
            None => Err(IpcError::core_not_ready(
                "核心后端未就绪：启动序列尚未完成（存储/管线注入前）",
            )),
        }
    }

    pub(crate) fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }
}

/// 构造即注入的 `OnceLock` 辅助。
fn backend_cell(backend: Arc<dyn IpcBackend>) -> OnceLock<Arc<dyn IpcBackend>> {
    let cell = OnceLock::new();
    let _ = cell.set(backend);
    cell
}
