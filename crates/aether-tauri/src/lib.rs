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

pub mod adapter_executor;
pub mod audit_bridge;
pub mod backup_control;
pub mod bindings;
pub mod config;
pub mod core_health;
pub mod diagnostics_control;
pub mod disk;
pub mod event_bridge;
pub mod ipc;
pub mod isolation;
pub mod json_payload;
pub mod logging;
pub mod nav;
pub mod permission_loop;
pub mod picker;
pub mod provider_control;
pub mod runtime_control;
pub mod runtime_registry;
pub mod security_level;
pub mod session_backend;
pub mod shutdown;
pub mod single_instance;
pub mod startup;

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
    // M3-05（D10/A3）：安全级别启动探针（OS 凭据库写→读→删自检；失败 → degraded），
    // 结果随 `startup_get` 快照供设置页只读展示（P0 不挂载加密文件，见模块说明）。
    gate.set_security_level(security_level::probe());
    let startup = std::sync::Arc::new(gate);
    #[cfg(debug_assertions)]
    startup_probe::record_phase(&startup);
    let startup_for_boot = std::sync::Arc::clone(&startup);

    // M2-07 DoD6（ADR-007 §5-1）：P0 运行期日志汇聚端接线（环形缓冲 + 数据目录文件）。
    // 启动门未就绪（同步盘阻断）时不触碰候选目录，仅环形缓冲；文件不可写同样退化。
    // M3-05：句柄保留并注入诊断后端（诊断包「日志汇聚产物」段）。
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

    // 路径白名单根目录（`validate_user_path` 配套；M3-05 起诊断导出走 ADR-003 决策 19
    // 外部路径语义，不再依赖该白名单——保持默认拒绝口径供白名单类命令复用）。
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
            let bundle = build_backend(&startup_for_boot, app.handle(), log_sink.clone());
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
fn build_backend(
    startup: &std::sync::Arc<startup::StartupGate>,
    app: &tauri::AppHandle,
    log_sink: Option<std::sync::Arc<logging::LogSink>>,
) -> BackendBundle {
    if startup.ensure_ready().is_err() {
        return BackendBundle {
            backend: std::sync::Arc::new(ipc::backend::NotImplementedBackend),
            shutdown: None,
        };
    }
    let data_dir = std::path::PathBuf::from(startup.snapshot().data_dir);
    let handle = tauri::async_runtime::handle().inner().clone();

    // M3-04（D13 七步第 3–6 步）：待处理恢复在存储打开**之前**执行（无写者窗口）；
    // 中断（kill -9）遗留的现场日志在此回滚（DoD4）。结果在核心启动后写审计。
    let restore_outcome = backup_control::boot_apply_pending_restore(&data_dir);

    // M2-07（ADR-007 §5-2）：监督器摘要接线在 health 之前完成，
    // `health.runtimes` 与监督器状态一一对应（字段映射冻结）。
    // M3-07（D9/SE-03）：审计出口经延迟观察者装配——监督器先于存储构造，
    // 存储就绪后注入 `StoreAuditObserver`（此前监督器无状态转移/审计活动）。
    // M4-05：安装产物内置只读注册清单（`resource_dir()/runtime-bundle/runtimes.json`；
    // ADR-015 §2.2 schema）→ 官方运行时注册；缺失/条目被拒不阻塞启动（空注册表告警）。
    let deferred_audit_observer = std::sync::Arc::new(audit_bridge::DeferredObserver::new());
    let registry = runtime_registry::resolve_registry_dir(app)
        .as_deref()
        .map(runtime_registry::load_registry)
        .unwrap_or_default();
    for (runtime_id, reason) in &registry.rejected {
        tracing::warn!(
            runtime_id = %runtime_id,
            reason = %reason,
            "运行时注册清单条目被拒（不阻塞启动；修复后重启重扫）"
        );
    }
    tracing::info!(
        registry_dir = ?registry.dir,
        registered = registry.specs.len(),
        rejected = registry.rejected.len(),
        manifest_present = registry.manifest_present,
        "运行时注册清单加载完成"
    );
    let supervisor: Option<std::sync::Arc<aether_adapters::supervisor::Supervisor>> =
        match runtime_control::boot_supervisor_with_observer(
            registry.specs,
            None,
            deferred_audit_observer.clone(),
        ) {
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

    // M3-02：会话后端（生命周期 + 适配器执行器 + 消息分页）随核心启动接线。
    let mut session_manager: Option<aether_control::SessionManager> = None;
    let mut adapter_executor: Option<std::sync::Arc<adapter_executor::AdapterRunExecutor>> = None;
    // M3-03：权限中心命令面（`permissions_pending`/`permission_resolve`）的服务句柄。
    let mut permission_service: Option<aether_control::PermissionService> = None;
    // M3-05：诊断后端依赖（成功分支注入真实健康/读/写/日志/任务 dump；降级分支注入降级快照）。
    let diagnostics_deps: Option<diagnostics_control::DiagnosticsDeps>;
    let runtimes_for_diagnostics = std::sync::Arc::clone(&runtimes);
    let (health, storage_slot, pipeline, reads, backup_deps, write_slot) =
        match core_health::boot_core_full(&data_dir, &handle, runtimes) {
            Ok(boot) => {
                let core_health::CoreBoot {
                    backend: core,
                    storage: slot,
                    reads,
                    write,
                    pipeline,
                } = boot;
                // M3-04（D13 第 7 步）：恢复请求/启动处理结果写审计（无待处理恢复则跳过）。
                if let Some(outcome) = &restore_outcome {
                    backup_control::write_boot_restore_audit(&write, &handle, outcome);
                }
                // M3-07（D9/SE-03）：适配器状态变化/监督审计落库出口接线（存储就绪后、
                // 启动序列尾段（孤儿清理/预热）前完成；此前监督器无回调活动）。
                deferred_audit_observer.set(std::sync::Arc::new(
                    audit_bridge::StoreAuditObserver::new(write.clone(), handle.clone()),
                ));
                // M3-04：备份/恢复命令面的存储句柄（读写队列均已就绪）。
                let backup_deps = (reads.clone(), write.clone());
                // M2-07 DoD3：核心 RSS 巡检（2GB 告警 / 2.5GB 强制 delta 限流；
                // env 钩子供测试/演练注入阈值）。任务随应用生命周期存活（detached）。
                {
                    let patrol = aether_control::ResourcePatrol::with_env();
                    let _patrol_task = patrol.start(pipeline.clone(), &handle);
                    // M3-01（D7/D8）：`aether://event` 事件桥接线（管线广播 → WebView 单通道）。
                    // 桥接只读转发；慢消费 `Lagged(k)` 不阻塞管线（见 event_bridge 模块说明）。
                    let metrics = event_bridge::spawn_app(app, &pipeline);
                    tracing::info!(
                        channel = bindings::EVENT_CHANNEL,
                        "事件桥已接线（采样计数见诊断）"
                    );
                    let _ = metrics;
                }
                // M3-03：权限中心真实接线（D9）——待审批清单/决议 IPC 的合法数据源。
                // 策略引擎先绑定数据目录（M3-03 兜底基准）；M3-08 起该基准与工作区绑定
                // 同源——启动恢复（`restore_workspace_binding`）与 `workspace_set` 会将
                // 同一 `PermissionService` 的策略引擎换根到工作区 canonical 路径。
                match aether_security::PolicyEngine::new(&data_dir) {
                    Ok(policy) => {
                        let service = aether_control::PermissionService::new(
                            aether_control::PermissionConfig::default(),
                            std::sync::Arc::new(aether_control::SystemClock),
                            policy,
                            write.clone(),
                            reads.clone(),
                            pipeline.clone(),
                        );
                        // D9：核心重启后待审批恢复（等待者随旧核心退出，仅恢复台账）。
                        match handle.block_on(service.restore_pending()) {
                            Ok(restored) => tracing::info!(
                                restored,
                                "权限待审批已恢复（D9：核心重启后 pending 恢复）"
                            ),
                            Err(error) => {
                                tracing::warn!(error = %error, "权限待审批恢复失败（继续启动）")
                            }
                        }
                        // D9：300s 超时巡检（deny + 审计；落盘成功才广播 permission.resolved）。
                        let background = service.spawn_background(&handle);
                        tracing::info!(background, "权限超时巡检已启动（300s → deny + 审计）");
                        permission_service = Some(service);
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "权限策略引擎初始化失败：权限中心命令回 core_not_ready（不伪造空队列）"
                        );
                    }
                }
                // M3-08 DoD7（关闭 M3-02 边界 9）：执行器权限网关由 `None` 切至核心
                // `PermissionService`（与 IPC 权限中心同一实例）——适配器 `permission.request`
                // 通知 100% 经策略矩阵/审批回环，零直通。
                let permission_gate = permission_service
                    .as_ref()
                    .map(|service| permission_loop::PermissionServiceGate::new(service.clone()));
                // M3-02：真实 run 执行器（适配器会话客户端 + M2-10 权限回环）。
                let executor = supervisor.as_ref().map(|supervisor| {
                    std::sync::Arc::new(adapter_executor::AdapterRunExecutor::new(
                        std::sync::Arc::clone(supervisor),
                        pipeline.clone(),
                        reads.clone(),
                        write.clone(),
                        handle.clone(),
                        permission_gate,
                    ))
                });
                let run_executor: std::sync::Arc<dyn aether_control::RunExecutor> = match &executor
                {
                    Some(executor) => executor.clone(),
                    None => std::sync::Arc::new(adapter_executor::UnavailableExecutor),
                };
                let manager = aether_control::SessionManager::new(
                    aether_control::LifecycleConfig::default(),
                    std::sync::Arc::new(aether_control::SystemClock),
                    write.clone(),
                    reads.clone(),
                    pipeline.clone(),
                    run_executor,
                );
                // M3-06 重启状态重建（D5「运行中崩溃 → 在途 run 标 failed，可重试」）：
                // 启动序列内、UI ready 握手前收口上一进程崩溃遗留的 queued/running run
                // 与非空闲会话（未确认不伪造完成；收口后可经 run_retry 重放）。
                match handle.block_on(manager.reconcile_interrupted_runs()) {
                    Ok(report) => tracing::info!(
                        runs_failed = report.runs_failed.len(),
                        sessions_reset = report.sessions_reset.len(),
                        "重启状态重建完成（未收口 run/会话已收口）"
                    ),
                    Err(error) => {
                        tracing::warn!(error = %error, "重启状态重建失败（继续启动；未收口项保留待下次）")
                    }
                }
                let background = manager.spawn_background(&handle);
                tracing::info!(background, "会话生命周期后台任务已启动（看门狗/事件监听）");
                session_manager = Some(manager);
                adapter_executor = executor;
                // M3-12/ADR-011：等待态写入接线（两阶段装配）——权限服务在「同会话
                // pending 票据计数 0↔1」时通知 `SessionManager`（观察者只上报；
                // 状态行与 `session.status_changed` 的唯一写入者仍是 SessionManager）。
                if let (Some(service), Some(manager)) = (&permission_service, &session_manager) {
                    service.set_pending_observer(std::sync::Arc::new(manager.clone()));
                }
                // M3-05（D11/D13）：诊断后端依赖——真实健康提供者 + 日志汇聚端 + 任务 dump
                // 源（M2-05 缓冲）+ 读/写句柄（库摘要/设置/提醒）。
                diagnostics_deps = Some(diagnostics_control::DiagnosticsDeps {
                    data_dir: data_dir.clone(),
                    reads: Some(reads.clone()),
                    write: Some(write.clone()),
                    handle: handle.clone(),
                    health: core_health::HealthProvider::new(
                        std::sync::Arc::new(pipeline.clone())
                            as std::sync::Arc<dyn core_health::PipelineHealthSource>,
                        runtimes_for_diagnostics,
                    ),
                    logs: log_sink.clone(),
                    task_dumps: session_manager.clone().map(|manager| {
                        std::sync::Arc::new(manager)
                            as std::sync::Arc<dyn diagnostics_control::TaskDumpSource>
                    }),
                    security: None,
                });
                (
                    std::sync::Arc::new(core) as std::sync::Arc<dyn ipc::IpcBackend>,
                    Some(slot),
                    Some(pipeline),
                    Some(reads),
                    Some(backup_deps),
                    Some(write),
                )
            }
            Err(error) => {
                tracing::error!(error = %error, "核心健康源启动失败：按只读降级呈现 health");
                let reason = error.to_string();
                // M3-05/D4：降级启动态诊断包仍可导出（健康快照/容量/日志；库摘要与配置缺失）。
                diagnostics_deps = Some(diagnostics_control::DiagnosticsDeps {
                    data_dir: data_dir.clone(),
                    reads: None,
                    write: None,
                    handle: handle.clone(),
                    health: core_health::degraded_provider(&reason),
                    logs: log_sink.clone(),
                    task_dumps: None,
                    security: None,
                });
                (
                    std::sync::Arc::new(core_health::degraded_backend(&reason))
                        as std::sync::Arc<dyn ipc::IpcBackend>,
                    None,
                    None,
                    None,
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
        let orchestrator = shutdown::AppShutdown::new(
            pipeline,
            supervisor.clone(),
            storage_slot,
            monitors,
            handle.clone(),
        );
        if let Some(manager) = &session_manager {
            orchestrator.install_session_manager(manager.clone());
        }
        // M3-03：退出序列先停权限超时巡检，再进入管线/存储关闭（D2 顺序不引入新阶段，
        // 仅复用既有「停止后台任务」段）。
        if let Some(service) = &permission_service {
            orchestrator.install_permission_service(service.clone());
        }
        Some(orchestrator)
    } else {
        None
    };

    let control: Option<std::sync::Arc<dyn runtime_control::RuntimeControl>> =
        supervisor.clone().map(|supervisor| {
            std::sync::Arc::new(runtime_control::SupervisorControl::new(
                supervisor,
                handle.clone(),
                runtime_control::RUNTIME_CONTROL_TIMEOUT,
            )) as std::sync::Arc<dyn runtime_control::RuntimeControl>
        });
    let inner: std::sync::Arc<dyn ipc::IpcBackend> =
        std::sync::Arc::new(runtime_control::RuntimeControlBackend::new(health, control));
    // M3-04（D13）：备份/恢复命令面装饰器（存储就绪时接线；降级启动时保留内层语义）。
    let inner: std::sync::Arc<dyn ipc::IpcBackend> = match backup_deps {
        Some((reads, write)) => std::sync::Arc::new(backup_control::BackupControlBackend::new(
            inner,
            data_dir.clone(),
            Some(reads),
            Some(write),
            handle.clone(),
        )),
        None => inner,
    };
    // M3-05（D11/D13）：诊断/容量装饰器——诊断包导出（脱敏 + 日志/健康/库摘要整合）、
    // `backup_list.reminder`（7 天未备份提醒）、已登记设置键读写。
    let inner: std::sync::Arc<dyn ipc::IpcBackend> = match diagnostics_deps {
        Some(deps) => std::sync::Arc::new(diagnostics_control::DiagnosticsControlBackend::new(
            inner, deps,
        )),
        None => inner,
    };
    let mut session_backend = session_backend::SessionBackend::new(
        inner,
        session_manager,
        adapter_executor,
        reads.clone(),
        supervisor,
        handle.clone(),
    );
    if let Some(service) = permission_service {
        session_backend = session_backend.with_permissions(service);
    }
    // M3-08：工作区写路径接线（`workspace_set` 落库）与启动恢复（最近绑定工作区 →
    // 新会话记忆注入 + 权限基准同源；旧会话不迁移）。
    if let Some(write) = write_slot.clone() {
        session_backend = session_backend.with_workspace_store(write);
    }
    // M3-11（ADR-010 决策 3/D10）：模型与供应商配置命令面接线。密钥存储启动选择：
    // keyring 自检通过 → OS 凭据库；否则 A3 降级加密文件（口令环境钩子，测试/演练）；
    // 两者皆不可用 → 密钥字段命令回诊断错误（不落明文）。
    if let (Some(reads), Some(write)) = (reads, write_slot) {
        let secrets = provider_control::boot_secret_store(&data_dir);
        session_backend = session_backend.with_providers(provider_control::ProviderControl::new(
            reads,
            write,
            secrets,
            handle.clone(),
        ));
    }
    match handle.block_on(session_backend.restore_workspace_binding()) {
        Ok(Some(binding)) => tracing::info!(
            workspace_id = %binding.id,
            root_path = %binding.root_path.display(),
            "工作区绑定已恢复（M3-08：权限基准与记忆注入同源）"
        ),
        Ok(None) => tracing::info!("无工作区绑定（workspace_set 前权限基准为数据目录）"),
        Err(error) => tracing::warn!(error = %error, "工作区绑定恢复失败（继续启动）"),
    }
    BackendBundle {
        backend: std::sync::Arc::new(session_backend),
        shutdown: orchestrator,
    }
}
