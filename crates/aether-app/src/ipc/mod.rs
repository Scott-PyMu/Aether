//! IPC 命令层（设计 D7）：命令面、严格参数校验框架与结构化错误。
//!
//! 安全基线（评审 #7）：
//! - 每个命令先经 [`validate::parse_strict`]（serde `deny_unknown_fields` +
//!   长度 / 枚举 / 格式校验）；
//! - 路径参数经 [`path::validate_user_path`]（canonicalize + 允许根目录前缀比较）；
//! - 校验失败返回 [`IpcError`]（结构化错误码），不调用后端、不落库、不透传下游。
//!
//! ADR-029 D-029-4：本模块是**唯一**命令实现（[`dispatch`] + [`COMMANDS`]）；
//! 桌面壳（Tauri/Electron）只做注册与 `State` 适配后调用 [`IpcState::dispatch`]。

pub mod backend;
pub mod business;
pub mod dispatch;
pub mod dto;
pub mod error;
pub mod handlers;
pub mod path;
pub mod validate;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub use backend::IpcBackend;
pub use business::BusinessCode;
pub use dispatch::{daemon_commands, dispatch, is_shell_local, COMMANDS, SHELL_COMMANDS};
pub use error::{IpcError, IpcErrorCode};

use crate::picker::DirectoryPicker;
use crate::shell_host::ShellHost;
use crate::shutdown::{AppShutdown, ExitFlag, ExitPhase};
use crate::startup::StartupGate;

/// 命令层共享状态：后端实现 + 路径白名单根目录 + 启动门（M1-06）
/// + 目录选择器（可测试性）+ 壳层宿主（退出/重启）+ 应用退出编排（M2-08）。
///
/// `backend` 为 `OnceLock`：生产启动在 Builder 阶段先以「延迟注入」构造
/// （窗口加载期状态必须可用——`startup_*` 门命令不依赖后端），
/// 单实例插件初始化之后由 `setup` 打开存储并注入真实后端（ADR-007；
/// T12 语义要求第二实例在 `setup` 前退出、不建写连接）。
///
/// `Clone` 为廉价句柄克隆（`Arc` 内部状态）：壳层可把状态移入阻塞线程池
/// （`spawn_blocking`）执行 [`IpcState::dispatch`]，不阻塞 UI 线程。
#[derive(Clone)]
pub struct IpcState {
    inner: Arc<IpcStateInner>,
}

struct IpcStateInner {
    backend: OnceLock<Arc<dyn IpcBackend>>,
    allowed_roots: Vec<PathBuf>,
    startup: Option<Arc<StartupGate>>,
    picker: OnceLock<Arc<dyn DirectoryPicker>>,
    /// 壳层宿主（`app_exit`/`app_restart`）；未注入 = 框架测试形态。
    shell: OnceLock<Arc<dyn ShellHost>>,
    /// M2-08 应用退出编排（核心启动完成后注入；未注入 = 放行直接退出）。
    shutdown: OnceLock<Arc<AppShutdown>>,
    /// M2-08 `ExitRequested` 防重入阶段。
    exit: ExitFlag,
}

impl IpcState {
    /// 未接线启动门的构造（M1-08 校验矩阵与框架测试用；生产见 [`Self::with_startup`]）。
    pub fn new(backend: Arc<dyn IpcBackend>, allowed_roots: Vec<PathBuf>) -> Self {
        Self::from_parts(backend_cell(backend), allowed_roots, None)
    }

    /// 生产构造：启动门生效后，未 Ready 时业务命令一律 `startup_blocked`。
    pub fn with_startup(
        backend: Arc<dyn IpcBackend>,
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
    ) -> Self {
        Self::from_parts(backend_cell(backend), allowed_roots, Some(startup))
    }

    /// 生产构造 + 预置目录选择器（E2E 注入固定路径 / 集成测试替身）。
    pub fn with_startup_and_picker(
        backend: Arc<dyn IpcBackend>,
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
        picker: Arc<dyn DirectoryPicker>,
    ) -> Self {
        let state = Self::with_startup(backend, allowed_roots, startup);
        let _ = state.inner.picker.set(picker);
        state
    }

    /// 生产构造（延迟后端）：Builder 阶段先 manage，`setup` 完成存储/管线后
    /// 经 [`Self::install_backend`] 注入（保持窗口加载期 `startup_*` 可用）。
    pub fn with_startup_deferred(allowed_roots: Vec<PathBuf>, startup: Arc<StartupGate>) -> Self {
        Self::from_parts(OnceLock::new(), allowed_roots, Some(startup))
    }

    /// 延迟构造 + 预置目录选择器（debug/E2E）。
    pub fn with_startup_deferred_and_picker(
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
        picker: Arc<dyn DirectoryPicker>,
    ) -> Self {
        let state = Self::with_startup_deferred(allowed_roots, startup);
        let _ = state.inner.picker.set(picker);
        state
    }

    fn from_parts(
        backend: OnceLock<Arc<dyn IpcBackend>>,
        allowed_roots: Vec<PathBuf>,
        startup: Option<Arc<StartupGate>>,
    ) -> Self {
        Self {
            inner: Arc::new(IpcStateInner {
                backend,
                allowed_roots,
                startup,
                picker: OnceLock::new(),
                shell: OnceLock::new(),
                shutdown: OnceLock::new(),
                exit: ExitFlag::new(),
            }),
        }
    }

    /// 注入真实后端（仅首次生效；返回 `false` 表示已注入）。
    pub fn install_backend(&self, backend: Arc<dyn IpcBackend>) -> bool {
        self.inner.backend.set(backend).is_ok()
    }

    /// 注入应用退出编排（M2-08；仅首次生效）。
    pub fn install_shutdown(&self, shutdown: Arc<AppShutdown>) -> bool {
        self.inner.shutdown.set(shutdown).is_ok()
    }

    /// 注入壳层宿主（退出/重启；仅首次生效）。
    pub fn install_shell(&self, shell: Arc<dyn ShellHost>) -> bool {
        self.inner.shell.set(shell).is_ok()
    }

    /// 注入目录选择器（生产 = 系统对话框；测试/E2E = 替身；仅首次生效）。
    pub fn set_picker(&self, picker: Arc<dyn DirectoryPicker>) -> bool {
        self.inner.picker.set(picker).is_ok()
    }

    /// 壳层宿主句柄（`None` = 未接线，框架测试形态）。
    pub fn shell(&self) -> Option<Arc<dyn ShellHost>> {
        self.inner.shell.get().cloned()
    }

    /// 退出编排句柄（`None` = 未接线，`ExitRequested` 直接放行）。
    pub fn shutdown_orchestrator(&self) -> Option<Arc<AppShutdown>> {
        self.inner.shutdown.get().cloned()
    }

    /// 退出阶段（壳层 `ExitRequested` 防重入判定）。
    pub fn exit_phase(&self) -> ExitPhase {
        self.inner.exit.phase()
    }

    /// `Idle → Running` 原子抢占。
    pub fn begin_exit_cleanup(&self) -> bool {
        self.inner.exit.begin()
    }

    /// 退出清理完成（放行第二次退出请求）。
    pub fn finish_exit_cleanup(&self) {
        self.inner.exit.finish();
    }

    pub fn startup(&self) -> Option<&StartupGate> {
        self.inner.startup.as_deref()
    }

    pub fn startup_arc(&self) -> Option<Arc<StartupGate>> {
        self.inner.startup.clone()
    }

    /// 目录选择器（生产：系统对话框；测试/E2E：注入替身）。
    pub fn picker(&self) -> Option<Arc<dyn DirectoryPicker>> {
        self.inner.picker.get().cloned()
    }

    /// 业务命令入口：启动门未 Ready 时阻断（主界面不可达的命令层兜底）；
    /// 后端尚未注入（启动序列未完成）时返回 `core_not_ready`（ADR-007 增量 2）。
    pub fn backend_ready(&self) -> Result<&dyn IpcBackend, IpcError> {
        if let Some(gate) = &self.inner.startup {
            gate.ensure_ready()?;
        }
        match self.inner.backend.get() {
            Some(backend) => Ok(&**backend),
            None => Err(IpcError::core_not_ready(
                "核心后端未就绪：启动序列尚未完成（存储/管线注入前）",
            )),
        }
    }

    /// 路径白名单根目录（`validate_user_path` 的配套访问器）。
    ///
    /// M3-05 起诊断导出改用 ADR-003 决策 19 的外部路径语义（选择器 + canonicalize +
    /// 空间护栏，不做默认目录信任），生产不再依赖本白名单；保留该访问器供
    /// 路径白名单类命令与测试复用（`validate_user_path` 仍为默认拒绝实现）。
    pub fn allowed_roots(&self) -> &[PathBuf] {
        &self.inner.allowed_roots
    }

    /// 命令面入口（ADR-029 D-029-4）：方法名 + 内层参数 → 结果 JSON。
    ///
    /// 壳层（Tauri/Electron/daemon）只做注册与传输适配；业务、校验与错误映射全部在此。
    /// [`COMMANDS`] 为 63 条命令清单；`method` 不在清单内返回 `not_implemented`
    /// （M9-03 的 JSON-RPC 层映射为 `method_not_found`）。
    pub fn dispatch(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, IpcError> {
        dispatch(self, method, params)
    }
}

/// 构造即注入的 `OnceLock` 辅助。
fn backend_cell(backend: Arc<dyn IpcBackend>) -> OnceLock<Arc<dyn IpcBackend>> {
    let cell = OnceLock::new();
    let _ = cell.set(backend);
    cell
}
