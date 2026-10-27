//! 壳层宿主抽象（ADR-029 D-029-2）。
//!
//! 应用层（本 crate）**不得**依赖 Tauri/Electron/窗口/对话框实现（AGENTS §2.1）；
//! 但 `app_exit` / `app_restart` / 备份恢复后续启属**壳层生命周期动作**。这里定义
//! 最小宿主接口，由壳层（`aether-tauri`，M9 后为 Electron preload→main）注入实现。
//!
//! 迁移期口径（M9「换壳不换功能」）：接口只覆盖既有命令语义所需的两个动作，
//! 不引入窗口控制/权限面。

/// 壳层宿主：退出与重启。
///
/// 实现方负责真正的进程生命周期（Tauri：`AppHandle::exit` / `request_restart`；
/// Electron：经 preload→main 转 `app_exit`/`app_restart`）。
pub trait ShellHost: Send + Sync + 'static {
    /// 请求退出应用（`app_exit`，`code` 为进程退出码）。
    fn exit(&self, code: i32);

    /// 请求重启应用（`app_restart` 与备份恢复后的 `restart_required`）。
    fn request_restart(&self);
}
