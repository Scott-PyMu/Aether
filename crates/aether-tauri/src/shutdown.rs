//! 壳层退出接线（ADR-029 D-029-2）：应用层退出编排 + Tauri `RunEvent` 适配。
//!
//! 编排对象 [`AppShutdown`] / 阶段 [`ExitPhase`] / 预算常量全部来自
//! `aether-app::shutdown`（本模块再导出）；本文件只保留 Tauri `ExitRequested`
//! 处理（`app_exit` 命令、窗口关闭、探针 `app.exit` 同路径）。

use aether_app::ipc::IpcState;

pub use aether_app::shutdown::*;

/// Tauri `RunEvent::ExitRequested` 处理（`app_exit` 命令、窗口关闭、探针 `app.exit` 同路径）。
///
/// - 未接线退出编排（启动门阻断/骨架测试）→ 不拦截，直接放行；
/// - 首次请求 → `prevent_exit` + 后台线程执行 [`AppShutdown::run_blocking`] → 完成后再
///   `exit(原始退出码)`（第二次请求经 [`ExitPhase::Done`] 放行）；
/// - 清理执行中的重复请求一律拦截（避免并发退出序列）。
pub fn on_exit_requested<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    code: Option<i32>,
    api: &tauri::ExitRequestApi,
) {
    use tauri::Manager;

    // 未接线退出编排（启动门阻断/骨架测试）→ 不拦截，直接放行。
    let Some(orchestrator) = app.state::<IpcState>().shutdown_orchestrator() else {
        return;
    };

    match app.state::<IpcState>().exit_phase() {
        ExitPhase::Done => return,
        ExitPhase::Running => {
            api.prevent_exit();
            return;
        }
        ExitPhase::Idle => {}
    }
    api.prevent_exit();
    if !app.state::<IpcState>().begin_exit_cleanup() {
        return;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let report = orchestrator.run_blocking();
        println!("{}", report.evidence_line());
        app.state::<IpcState>().finish_exit_cleanup();
        app.exit(code.unwrap_or(0));
    });
}
