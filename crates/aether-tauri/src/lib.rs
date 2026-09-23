//! Aether 桌面壳（设计 D1：Tauri 2；D2：命令层保持薄，核心逻辑不在此 crate）。
//!
//! M1-08 起落地 Tauri 安全基线（设计 D7 / 评审 #7）：
//! - CSP 由 `tauri.conf.json` 冻结（[`config::EXPECTED_CSP`]），并由 E2E 断言真实阻断；
//! - `withGlobalTauri:false`，非本地导航经 [`nav`] 拦截并转交系统浏览器；
//! - IPC 命令面经 [`ipc`] 的统一参数校验框架（严格反序列化 + 路径 canonicalize +
//!   枚举白名单 + 长度上限），校验失败返回结构化错误、不落库、不透传下游；
//! - capabilities 最小 allowlist 签入（[`config`] 断言），devtools 仅 debug 可达。
//!
//! 测试统一位于 `tests/`（见 `Cargo.toml` 的说明）。

// 核心 crate 禁止 unwrap/expect/panic（AGENTS §2.2）；lib 内单元测试显式豁免
// （与 aether-core/store/adapters/control 同口径；集成测试各自在文件级豁免）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod core_health;
pub mod ipc;
pub mod isolation;
pub mod logging;
pub mod nav;
pub mod permission_loop;
pub mod picker;
pub mod runtime_control;
pub mod shutdown;
pub mod single_instance;
pub mod startup;

#[cfg(debug_assertions)]
mod health_probe;
#[cfg(debug_assertions)]
mod probe;
#[cfg(debug_assertions)]
mod startup_probe;

/// 产品名（与 `tauri.conf.json` 的 productName 一致）。
pub const APP_NAME: &str = "Aether";

/// 应用版本号——单一版本来源（工作区 `Cargo.toml`，构建时注入）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 核心层版本号（诊断与冒烟测试使用）。
pub fn core_version() -> &'static str {
    aether_core::version()
}

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
/// 2. `setup` 阶段（即单实例插件初始化、第二实例退出之后）打开存储/管线并注入真实后端。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let gate = startup::StartupGate::bootstrap();
    // E2E 探针可注入指针写入失败（复现「复制完成、写指针失败」窗口）。
    #[cfg(debug_assertions)]
    let gate = startup_probe::maybe_override_pointer_writer(gate);
    let startup = std::sync::Arc::new(gate);
    #[cfg(debug_assertions)]
    startup_probe::record_phase(&startup);
    let startup_for_boot = std::sync::Arc::clone(&startup);

    // M2-07 DoD6（ADR-007 §5-1）：P0 运行期日志汇聚端接线（环形缓冲 + 数据目录文件）。
    // 启动门未就绪（同步盘阻断）时不触碰候选目录，仅环形缓冲；文件不可写同样退化。
    match startup.snapshot().phase {
        startup::StartupPhase::Ready => {
            match logging::init_for_data_dir(std::path::Path::new(&startup.snapshot().data_dir)) {
                Ok(_sink) => {}
                Err(error) => eprintln!("[aether] {error}（继续以无汇聚端运行）"),
            }
        }
        _ => {
            if logging::init(logging::LogSink::new(logging::DEFAULT_RING_CAPACITY)).is_err() {
                eprintln!("[aether] 日志汇聚端接线失败（继续以无汇聚端运行）");
            }
        }
    }

    // 路径白名单根目录随 M3-05（诊断导出）接入；未配置即默认拒绝。
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
            let bundle = build_backend(&startup_for_boot);
            let _ = app.state::<ipc::IpcState>().install_backend(bundle.backend);
            // M2-08：应用退出编排接线（存储五步 + 适配器终止段；未接线则退出直接放行）。
            if let Some(orchestrator) = bundle.shutdown {
                let _ = app.state::<ipc::IpcState>().install_shutdown(orchestrator);
            }
            app.state::<ipc::IpcState>()
                .set_app_handle(app.handle().clone());
            #[cfg(debug_assertions)]
            {
                probe::setup_window(app.handle())?;
                startup_probe::start(app.handle().clone());
                health_probe::start(app.handle().clone());
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

/// 后端装配结果：命令后端 + 应用退出编排（M2-08）。
struct BackendBundle {
    backend: std::sync::Arc<dyn ipc::IpcBackend>,
    shutdown: Option<std::sync::Arc<shutdown::AppShutdown>>,
}

/// 构造命令后端（ADR-007 `health` 真实接线 + M2-01 `runtime_*` 接线 + M2-07 巡检
/// + M2-08 启动序列与退出编排）。
///
/// - 启动门 Ready：监督器注册表（台账）先接线（`health.runtimes` 返回真实快照：
///   空注册表 `[]`；台账初始化失败 → `null`）→ 打开存储（`quick_check` + 迁移）→
///   事件管线 → [`core_health::CoreHealthBackend`] → **启动序列尾段（M2-08/D2）**：
///   孤儿清理（D5 三条件）→ 适配器预热 → 心跳监控接线 → 退出编排注入；RSS 巡检
///   （D2：2GB 告警 / 2.5GB 限流）随核心启动；
/// - 监督器（M2-01 空注册表；M2-02 注册真实适配器）：接线 `runtime_retry`/`runtime_enable`；
/// - 启动失败（安全模式等）：按 D3 只读语义呈现为 `persist_degraded`（`degraded_backend`），
///   不回退 `not_implemented`；巡检不启动（无管线句柄）；退出编排仅保留已就绪的监督器；
/// - 启动门阻断（A4 同步盘检测）：核心不启动；业务命令由启动门返回 `startup_blocked`。
fn build_backend(startup: &std::sync::Arc<startup::StartupGate>) -> BackendBundle {
    if startup.ensure_ready().is_err() {
        return BackendBundle {
            backend: std::sync::Arc::new(ipc::backend::NotImplementedBackend),
            shutdown: None,
        };
    }
    let data_dir = std::path::PathBuf::from(startup.snapshot().data_dir);
    let handle = tauri::async_runtime::handle().inner().clone();

    // M2-07（ADR-007 §5-2）：监督器摘要接线在 health 之前完成，
    // `health.runtimes` 与监督器状态一一对应（字段映射冻结）。
    let supervisor: Option<std::sync::Arc<aether_adapters::supervisor::Supervisor>> =
        match runtime_control::boot_empty_supervisor(None) {
            Ok(supervisor) => Some(std::sync::Arc::new(supervisor)),
            Err(error) => {
                tracing::warn!(error = %error, "监督器台账初始化失败：runtime 控制命令回 core_not_ready");
                None
            }
        };
    let runtimes: std::sync::Arc<dyn core_health::RuntimeSummarySource> = match &supervisor {
        Some(supervisor) => std::sync::Arc::new(core_health::SupervisorRuntimeSummaries::new(
            std::sync::Arc::clone(supervisor),
        )),
        None => std::sync::Arc::new(core_health::StaticRuntimeSummaries::unwired()),
    };

    let (health, storage_slot, pipeline) =
        match core_health::boot_core_health_with_slot(&data_dir, &handle, runtimes) {
            Ok((core, slot)) => {
                // M2-07 DoD3：核心 RSS 巡检（2GB 告警 / 2.5GB 强制 delta 限流；
                // env 钩子供测试/演练注入阈值）。任务随应用生命周期存活（detached）。
                if let Some(pipeline) = core.pipeline() {
                    let patrol = aether_control::ResourcePatrol::with_env();
                    let _patrol_task = patrol.start((**pipeline).clone(), &handle);
                }
                let pipeline = core.pipeline().map(|pipeline| (**pipeline).clone());
                (
                    std::sync::Arc::new(core) as std::sync::Arc<dyn ipc::IpcBackend>,
                    Some(slot),
                    pipeline,
                )
            }
            Err(error) => {
                tracing::error!(error = %error, "核心健康源启动失败：按只读降级呈现 health");
                (
                    std::sync::Arc::new(core_health::degraded_backend(&error.to_string()))
                        as std::sync::Arc<dyn ipc::IpcBackend>,
                    None,
                    None,
                )
            }
        };

    // M2-08 启动序列尾段（D2：库打开 + quick_check → 孤儿清理 → 迁移/预热）：
    // 清理台账残留（强杀核心后的孤儿）；预热 enabled 适配器；接线心跳监控（T5b）。
    let monitors = match &supervisor {
        Some(supervisor) => match runtime_control::run_supervisor_startup(supervisor, &handle) {
            Ok(startup_report) => {
                let reclaimed = startup_report.cleanup.reclaimed().count();
                let skipped = startup_report.cleanup.skipped().count();
                tracing::info!(
                    reclaimed,
                    skipped,
                    actions = startup_report.cleanup.actions.len(),
                    "启动孤儿清理完成（D5 台账三条件）"
                );
                for (runtime_id, outcome) in &startup_report.warmups {
                    tracing::info!(runtime_id = %runtime_id, outcome = ?outcome, "适配器预热结果");
                }
                startup_report.monitors
            }
            Err(error) => {
                tracing::warn!(error = %error, "启动序列尾段失败（孤儿清理/预热未完成）");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let orchestrator = if supervisor.is_some() || pipeline.is_some() || storage_slot.is_some() {
        Some(shutdown::AppShutdown::new(
            pipeline,
            supervisor.clone(),
            storage_slot,
            monitors,
            handle.clone(),
        ))
    } else {
        None
    };

    let control: Option<std::sync::Arc<dyn runtime_control::RuntimeControl>> =
        supervisor.map(|supervisor| {
            std::sync::Arc::new(runtime_control::SupervisorControl::new(
                supervisor,
                handle,
                runtime_control::RUNTIME_CONTROL_TIMEOUT,
            )) as std::sync::Arc<dyn runtime_control::RuntimeControl>
        });
    BackendBundle {
        backend: std::sync::Arc::new(runtime_control::RuntimeControlBackend::new(health, control)),
        shutdown: orchestrator,
    }
}
