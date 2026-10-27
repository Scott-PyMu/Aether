//! Aether 桌面壳（设计 D1：Tauri 2；D2：命令层保持薄，核心逻辑不在此 crate）。
//!
//! **M9-02（ADR-029 D-029-2/D-029-4）**：业务层已抽到 `aether-app`（DTO、错误、
//! 严格校验、路径、picker trait、启动门、后端装配与命令实现）。本 crate 只剩壳适配：
//! - `ipc::commands`：63 个 Tauri wrapper → `IpcState::dispatch`（注册 + `State` 适配）；
//! - `picker` / `shell_host` / `shutdown` / `runtime_registry`：Tauri 能力实现；
//! - `event_bridge` / `bindings` / `nav` / `single_instance` / debug 探针：壳内组件。
//!
//! 迁移期既有模块路径（`aether_tauri::session_backend` 等）经 `pub use aether_app::*`
//! 保持可用，集成测试与 E2E 不因抽取而失效。
//!
//! 安全基线（设计 D7 / 评审 #7）不变：CSP 由 `tauri.conf.json` 冻结；`withGlobalTauri:false`；
//! 非本地导航经 [`nav`] 拦截；IPC 参数严格校验（实现已移至 `aether-app::ipc::validate`）。

// 核心 crate 禁止 unwrap/expect/panic（AGENTS §2.2）；lib 内单元测试显式豁免
// （与 aether-core/store/adapters/control 同口径；集成测试各自在文件级豁免）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod bindings;
pub mod event_bridge;
pub mod ipc;
pub mod nav;
pub mod picker;
pub mod runtime_registry;
pub mod shell_host;
pub mod shutdown;
pub mod single_instance;

#[cfg(debug_assertions)]
mod health_probe;
#[cfg(debug_assertions)]
mod m3_06_probe;
#[cfg(debug_assertions)]
mod m4_05_probe;
#[cfg(debug_assertions)]
mod probe;
#[cfg(debug_assertions)]
mod startup_probe;

// 迁移期路径兼容（ADR-029 D-029-2）：应用层模块原样再导出。
pub use aether_app::{
    adapter_executor, archive_control, audit_bridge, backup_auto, backup_control, config,
    core_health, credential_broker, diagnostics_control, disk, isolation, json_payload, logging,
    permission_loop, preset_control, provider_catalog, provider_connectivity, provider_control,
    provider_env, registry_identity, retention, runtime_control, security_level, session_backend,
    startup,
};

use aether_app::boot;

/// 产品名、应用版本与核心版本（迁移期路径兼容：定义在 `aether-app`）。
pub use aether_app::{core_version, version, APP_NAME};

/// 处理「打印信息后退出」类 CLI 参数（CI 冒烟与安装后自检使用）。
///
/// 返回 `Some(exit_code)` 表示参数已被处理、调用方应立即退出；
/// 返回 `None` 表示继续启动 GUI。
#[must_use]
pub fn cli_exit_code<I, S>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for arg in args {
        match arg.as_ref() {
            "--version" | "-V" => {
                println!("{APP_NAME} {}", version());
                return Some(0);
            }
            "--aether-diagnostics" => {
                println!("{APP_NAME} {} (core {})", version(), core_version());
                return Some(0);
            }
            _ => {}
        }
    }
    None
}

/// 启动 Tauri 应用。
///
/// 启动序列（设计 D1/D2）：单实例锁 → 数据目录检测（A4）→ 库打开 + `quick_check` →
/// 事件管线（ADR-007 `health` 接线）→ …。检测命中时启动门进入 `BlockedSyncDir`：
/// 业务命令全部 `startup_blocked`，UI 只渲染「迁移到本地目录 / 退出」。
///
/// 状态管理分两步（保证窗口加载期与 T12 顺序）：
/// 1. Builder 阶段以「延迟后端」`manage` 状态（`startup_*` 门命令不依赖后端，窗口加载期可用）；
/// 2. `setup` 阶段（即单实例插件初始化、第二实例退出之后）调用 `aether-app` 的
///    [`boot::build_backend`] 打开存储/管线并注入真实后端；本层负责事件桥与探针接线。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let gate = startup::StartupGate::bootstrap();
    // E2E 探针可注入指针写入失败（复现「复制完成、写指针失败」窗口）。
    #[cfg(debug_assertions)]
    let gate = startup_probe::maybe_override_pointer_writer(gate);
    // M3-05（D10/A3）：安全级别启动探针（OS 凭据库写→读→删自检；失败 → degraded）。
    gate.set_security_level(security_level::probe());
    let startup = std::sync::Arc::new(gate);
    #[cfg(debug_assertions)]
    startup_probe::record_phase(&startup);
    let startup_for_boot = std::sync::Arc::clone(&startup);

    // M2-07 DoD6（ADR-007 §5-1）：P0 运行期日志汇聚端接线（环形缓冲 + 数据目录文件）。
    let log_sink: Option<std::sync::Arc<logging::LogSink>> = match startup.snapshot().phase {
        startup::StartupPhase::Ready => {
            match logging::init_for_data_dir(std::path::Path::new(&startup.snapshot().data_dir)) {
                Ok(sink) => Some(sink),
                Err(error) => {
                    eprintln!("[aether] {error}（继续以无汇聚端运行）");
                    None
                }
            }
        }
        _ => {
            let sink = logging::LogSink::new(logging::DEFAULT_RING_CAPACITY);
            if logging::init(std::sync::Arc::clone(&sink)).is_err() {
                eprintln!("[aether] 日志汇聚端接线失败（继续以无汇聚端运行）");
            }
            Some(sink)
        }
    };

    // debug + E2E 探针可注入固定目录选择器（迁移主路径自动化）；生产用系统选择器。
    #[cfg(debug_assertions)]
    let state = match startup_probe::injected_picker() {
        Some(picker) => {
            ipc::IpcState::with_startup_deferred_and_picker(Vec::new(), startup, picker)
        }
        None => ipc::IpcState::with_startup_deferred(Vec::new(), startup),
    };
    #[cfg(not(debug_assertions))]
    let state = ipc::IpcState::with_startup_deferred(Vec::new(), startup);

    tauri::Builder::default()
        // T12：single-instance 必须是第一个注册的插件（第二实例转发后即退出）。
        .plugin(single_instance::plugin())
        .plugin(tauri_plugin_dialog::init())
        .plugin(nav::plugin())
        .invoke_handler(ipc::handler())
        .manage(state)
        .setup(move |app| {
            use tauri::Manager;

            // 单实例插件已初始化：此处才做「库打开 + quick_check」与管线启动（D2 顺序），
            // 并注入已 manage 的状态（窗口加载期状态始终可用）。
            let runtime = tauri::async_runtime::handle();
            let bundle = boot::build_backend(
                &startup_for_boot,
                runtime_registry::resource_dir(app.handle()),
                log_sink.clone(),
                runtime.inner(),
            );
            let ipc_state = app.state::<ipc::IpcState>();
            let _ = ipc_state.install_backend(bundle.backend);
            // M2-08：应用退出编排接线（存储五步 + 适配器终止段；未接线则退出直接放行）。
            if let Some(orchestrator) = bundle.shutdown {
                let _ = ipc_state.install_shutdown(orchestrator);
            }
            // ADR-029 D-029-2：壳层宿主与目录选择器注入（应用层只依赖 trait）。
            let _ = ipc_state.install_shell(std::sync::Arc::new(shell_host::TauriShellHost::new(
                app.handle().clone(),
            )));
            let _ = ipc_state.set_picker(std::sync::Arc::new(picker::TauriDialogPicker::new(
                app.handle().clone(),
            )));
            // M3-01（D7/D8）：`aether://event` 事件桥接线（管线广播 → WebView 单通道）。
            if let Some(pipeline) = &bundle.pipeline {
                let _metrics = event_bridge::spawn_app(app.handle(), pipeline);
                tracing::info!(
                    channel = bindings::EVENT_CHANNEL,
                    "事件桥已接线（采样计数见诊断）"
                );
            }
            #[cfg(debug_assertions)]
            {
                probe::setup_window(app.handle())?;
                startup_probe::start(app.handle().clone());
                health_probe::start(app.handle().clone());
                // M3-06：降级恢复 E2E 探针（降级横幅 → app_restart → 重启后自检）。
                m3_06_probe::start(
                    app.handle().clone(),
                    std::path::PathBuf::from(&startup_for_boot.snapshot().data_dir),
                );
                // M4-05：真实 WebView 内联权限回环 E2E 探针（授权卡 → 允许 → 适配器回执）。
                m4_05_probe::start(app.handle().clone());
            }
            #[cfg(not(debug_assertions))]
            {
                let _ = app;
            }
            Ok(())
        })
        .build(tauri::generate_context!())?
        // M2-08：D2 关闭序列在应用退出路径收口（`app_exit` 命令 / 窗口关闭 / 探针退出
        // 同路径）；执行完成前 prevent_exit，完成后以原始退出码退出。
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                shutdown::on_exit_requested(app_handle, code, &api);
            }
        });
    Ok(())
}
