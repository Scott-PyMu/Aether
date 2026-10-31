//! 核心装配（ADR-029 D-029-2）：transport-agnostic 的后端构建与启动序列尾段。
//!
//! 壳层（Tauri/Electron/daemon）只负责：解析资源目录、注入日志汇聚端、提供 tokio 句柄、
//! 把 [BackendBundle] 安装到 [crate::ipc::IpcState]，并按需接线自己的事件通道。

use crate::*;

/// 后端装配结果：命令后端 + 应用退出编排（M2-08）。
pub struct BackendBundle {
    pub backend: std::sync::Arc<dyn ipc::IpcBackend>,
    pub shutdown: Option<std::sync::Arc<shutdown::AppShutdown>>,
    /// 事件管线句柄（壳层据此接线 ether://event 桥；无核心/降级启动为 None）。
    pub pipeline: Option<aether_control::EventPipeline>,
    /// 单写队列句柄（事件桥升级审计出口；无核心/降级启动为 None）。
    pub write: Option<aether_store::WriteQueue>,
    /// M9-09：Keychain preflight（只读；无密钥存储/降级启动为 None）。
    pub keychain_preflight: Option<crate::keychain_preflight::KeychainPreflightReport>,
}

/// M9-12（ADR-029 §7/§8.2 P2-10）：权限服务配置。
///
/// 生产路径恒为 D9 的 300s；仅 `test-hooks` feature 构建允许把审批超时压缩到
/// 可测范围（Playwright E2E 的 permission timeout 场景）。该钩子**不注册命令**，
/// 也不进入 `command-manifest.json`；普通 debug/release 构建忽略环境变量。
fn permission_config() -> aether_control::PermissionConfig {
    #[cfg(feature = "test-hooks")]
    {
        if let Ok(raw) = std::env::var("AETHER_TEST_PERMISSION_TIMEOUT_MS") {
            if let Ok(timeout_ms) = raw.parse::<i64>() {
                if timeout_ms > 0 {
                    let sweep_tick_ms = (timeout_ms / 10).clamp(50, 1_000);
                    return aether_control::PermissionConfig {
                        ask_timeout_ms: timeout_ms,
                        sweep_tick: std::time::Duration::from_millis(sweep_tick_ms as u64),
                        wait_timeout: Some(std::time::Duration::from_millis(
                            (timeout_ms as u64).saturating_add(5_000),
                        )),
                    };
                }
            }
            tracing::warn!(
                value = %raw,
                "AETHER_TEST_PERMISSION_TIMEOUT_MS 非法（test-hooks）；回退 300s"
            );
        }
    }
    aether_control::PermissionConfig::default()
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
pub fn build_backend(
    startup: &std::sync::Arc<startup::StartupGate>,
    resource_dir: Option<std::path::PathBuf>,
    log_sink: Option<std::sync::Arc<logging::LogSink>>,
    runtime: &tokio::runtime::Handle,
) -> BackendBundle {
    if startup.ensure_ready().is_err() {
        return BackendBundle {
            backend: std::sync::Arc::new(ipc::backend::NotImplementedBackend),
            shutdown: None,
            pipeline: None,
            write: None,
            keychain_preflight: None,
        };
    }
    let data_dir = std::path::PathBuf::from(startup.snapshot().data_dir);
    let handle = runtime.clone();

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
    // M7-06/ADR-024：供应商消费映射 spawn 环境注入点（存储/密钥存储就绪后接线；
    // 每次 spawn 现取——无匹配 provider / 读取失败 → 不注入，不阻断启动）。
    let provider_env_hook = provider_env::ProviderEnvHook::new(data_dir.clone());
    // ADR-028 v0.9：`provider.catalog.get` 数据源（adapter → core；存储就绪前返回
    // core_not_ready，启动尾段接线）。
    let provider_catalog = std::sync::Arc::new(provider_catalog::ProviderCatalog::new());
    let registry = runtime_registry::resolve_registry_dir(resource_dir)
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
    let mut specs = registry.specs;
    for spec in &mut specs {
        spec.env_source = Some(provider_env_hook.clone());
        spec.catalog_source = Some(provider_catalog.clone()
            as std::sync::Arc<dyn aether_adapters::connection::ProviderCatalogSource>);
    }
    let supervisor: Option<std::sync::Arc<aether_adapters::supervisor::Supervisor>> =
        match runtime_control::boot_supervisor_with_observer(
            specs,
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
    // M7-02/ADR-022：并发闸门快照单元（`health.concurrency_gate` 同步读；管理器单写）。
    let concurrency_gate = aether_control::ConcurrencyGateCell::new();
    // M3-05：诊断后端依赖（成功分支注入真实健康/读/写/日志/任务 dump；降级分支注入降级快照）。
    let diagnostics_deps: Option<diagnostics_control::DiagnosticsDeps>;
    let runtimes_for_diagnostics = std::sync::Arc::clone(&runtimes);
    let (health, storage_slot, pipeline, reads, backup_deps, write_slot) =
        match core_health::boot_core_full_with_gate(
            &data_dir,
            &handle,
            runtimes,
            Some(concurrency_gate.clone()),
        ) {
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
                // M7-01/ADR-015（DoD0）：同时注入事件管线——每次合法状态转移发射
                // `runtime.status_changed`（合成归段 `runtime:<id>`；先日志后广播）。
                deferred_audit_observer.set(std::sync::Arc::new(
                    audit_bridge::StoreAuditObserver::new(write.clone(), handle.clone())
                        .with_pipeline(pipeline.clone()),
                ));
                // M7-01/ADR-015 决策 4：注册清单条目拒绝 → 审计 `runtime.registry_rejected`
                // （存储就绪后回放；此前仅有 tracing 告警；不阻塞启动）。
                for (runtime_id, reason) in &registry.rejected {
                    if let Ok(id) = aether_core::RuntimeId::new(runtime_id.clone()) {
                        aether_adapters::supervisor::SupervisorObserver::on_audit(
                            deferred_audit_observer.as_ref(),
                            &aether_adapters::supervisor::AuditRecord::new(
                                id,
                                aether_adapters::supervisor::AuditKind::RegistryRejected,
                                reason.clone(),
                                aether_adapters::supervisor::now_ms(),
                            ),
                        );
                    }
                }
                // M3-04：备份/恢复命令面的存储句柄（读写队列均已就绪）。
                let backup_deps = (reads.clone(), write.clone());
                // M2-07 DoD3：核心 RSS 巡检（2GB 告警 / 2.5GB 强制 delta 限流；
                // env 钩子供测试/演练注入阈值）。任务随应用生命周期存活（detached）。
                {
                    let patrol = aether_control::ResourcePatrol::with_env();
                    let _patrol_task = patrol.start(pipeline.clone(), &handle);
                    // ADR-029 D-029-2：事件桥由壳层接线（本 crate 不感知 Tauri 事件通道）。
                }
                // M3-03：权限中心真实接线（D9）——待审批清单/决议 IPC 的合法数据源。
                // 策略引擎先绑定数据目录（M3-03 兜底基准）；M3-08 起该基准与工作区绑定
                // 同源——启动恢复（`restore_workspace_binding`）与 `workspace_set` 会将
                // 同一 `PermissionService` 的策略引擎换根到工作区 canonical 路径。
                match aether_security::PolicyEngine::new(&data_dir) {
                    Ok(policy) => {
                        let service = aether_control::PermissionService::new(
                            permission_config(),
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
                        // M6-01/ADR-019：持久授权（session/always）与用户规则重启重建
                        // （历史行 canonicalize 失败 → 不命中并告警，fail-closed）。
                        match handle.block_on(service.restore_grants()) {
                            Ok(restored) => {
                                tracing::info!(restored, "持久授权匹配集已重建（ADR-019 附录 A-2）")
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "持久授权重建失败（继续启动）")
                            }
                        }
                        match handle.block_on(service.restore_rules()) {
                            Ok(loaded) => {
                                tracing::info!(loaded, "用户权限规则快照已加载（ADR-019）")
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "规则快照加载失败（继续启动）")
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
                let mut manager = aether_control::SessionManager::new(
                    aether_control::LifecycleConfig::default(),
                    std::sync::Arc::new(aether_control::SystemClock),
                    write.clone(),
                    reads.clone(),
                    pipeline.clone(),
                    run_executor,
                )
                // M7-02/ADR-022：并发闸门——空闲内存采样（启动即采样 + 5s 巡检；
                // `clamp(floor(free_ram_gb/1.5), 2, 10)`）与 health 快照单元共享。
                .with_free_ram(std::sync::Arc::new(aether_control::SysinfoFreeRam::new()))
                .with_concurrency_gate(concurrency_gate.clone());
                // M7-03/ADR-017：`session.pause` 线协议下发钩子（受控中断收口后
                // best-effort 通知适配器；未接线/无适配器会话 = 跳过）。
                if let Some(executor) = &executor {
                    let notifier: std::sync::Arc<dyn aether_control::PauseNotifier> =
                        executor.clone();
                    manager = manager.with_pause_notifier(notifier);
                }
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
                    )
                    .with_concurrency_gate(concurrency_gate.clone()),
                    logs: log_sink.clone(),
                    task_dumps: session_manager.clone().map(|manager| {
                        std::sync::Arc::new(manager)
                            as std::sync::Arc<dyn diagnostics_control::TaskDumpSource>
                    }),
                    // M7-03/ADR-017：暂停统计（诊断包 `session_pause` 段；
                    // 暂停次数/恢复次数/当前暂停会话数）。
                    session_manager: session_manager.clone(),
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
                    session_manager: None,
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
    // 清理台账残留（强杀核心后的孤儿）；按持久化偏好预热（M7-01/ADR-015 决策 5：
    // `enabled=0` 不 spawn、置 `cold → disabled(user_disabled)`）；接线心跳监控（T5b）。
    // M7-06/ADR-024 / ADR-028：存储/密钥存储就绪后**先接线**，再预热适配器——
    // 否则首次 spawn 拿不到供应商 env/broker（只有后续 enable/retry 才生效）。
    let boot_secrets = if backup_deps.is_some() {
        provider_control::boot_secret_store(&data_dir)
    } else {
        None
    };
    if let Some(reads) = reads.as_ref() {
        provider_env_hook.wire(reads.clone(), boot_secrets.clone());
        provider_catalog.wire(reads.clone());
    }
    // M9-09 DoD-8：Keychain 启动 preflight（只读探测旧 Tauri 条目；绝不删除）。
    // 旧条目在新签名身份下不可读（或密钥存储未挂载）→ 报告 `mode=reauth`；daemon
    // 预热完成后经 `daemon.status.reason=keychain_reauth_required` 通知 main 显示 S-13。
    let keychain_preflight = if backup_deps.is_some() {
        let provider_ids = match reads.as_ref() {
            Some(reads) => match handle.block_on(reads.providers()) {
                Ok(providers) => providers
                    .into_iter()
                    .filter(|provider| provider.api_key_ref.is_some())
                    .map(|provider| provider.id)
                    .collect(),
                Err(error) => {
                    tracing::warn!(error = %error, "Keychain preflight 读取供应商清单失败（跳过条目探测）");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let store = boot_secrets.as_ref().map(|store| store.as_ref() as &dyn aether_security::SecretStore);
        let report = crate::keychain_preflight::run_boot(store, &provider_ids);
        tracing::info!(
            scope = %report.scope,
            mode = %report.mode,
            readable = report.readable,
            missing = report.missing,
            unavailable = report.unavailable,
            "Keychain preflight 完成（只读；不删除旧条目）"
        );
        Some(report)
    } else {
        None
    };
    // ADR-028：本地凭据 broker（仅本机通道；失败时适配器回退单供应商 env）。
    if let Some(info) = credential_broker::start(provider_env_hook.clone(), &data_dir, &handle) {
        provider_env_hook.set_broker_info(info);
    }

    let monitors = match &supervisor {
        Some(supervisor) => {
            let prefs = match &reads {
                Some(reads) => match handle.block_on(reads.runtime_preferences()) {
                    Ok(prefs) => prefs,
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "运行时启停偏好读取失败（按构建期缺省预热；不阻塞启动）"
                        );
                        std::collections::BTreeMap::new()
                    }
                },
                None => std::collections::BTreeMap::new(),
            };
            match runtime_control::run_supervisor_startup_with_prefs(supervisor, &handle, &prefs) {
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
            }
        }
        None => Vec::new(),
    };

    // 壳层需要管线句柄接线事件桥（`AppShutdown::new` 会取走管线所有权）；
    // 升级审计同样需要单写队列（后段 `write_slot` 会被装饰器消费，先克隆）。
    let pipeline_for_bridge = pipeline.clone();
    let write_for_bridge = write_slot.clone();
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
            let control = runtime_control::SupervisorControl::new(
                supervisor,
                handle.clone(),
                runtime_control::RUNTIME_CONTROL_TIMEOUT,
            );
            // M7-01/ADR-015：注入启停偏好与在途计数（`runtime_disable` 确认门需要
            // 存储接线；`runtime_enable` 偏好写序）。存储未就绪（降级启动）时保持
            // 无存储构造：disable 回 `core_not_ready`，retry/enable 沿用既有语义。
            let control = match (&reads, &write_slot) {
                (Some(reads), Some(write)) => {
                    control.with_runtime_store(reads.clone(), write.clone())
                }
                _ => control,
            };
            std::sync::Arc::new(control) as std::sync::Arc<dyn runtime_control::RuntimeControl>
        });
    let inner: std::sync::Arc<dyn ipc::IpcBackend> =
        std::sync::Arc::new(runtime_control::RuntimeControlBackend::new(health, control));
    // M3-04（D13）：备份/恢复命令面装饰器（存储就绪时接线；降级启动时保留内层语义）。
    // M5-02（D13/ADR-020）：自动备份调度器随备份后端接线（后台任务，不阻塞主流程）；
    // 注册进退出编排，退出期间不再触发新备份。
    // M5-03（D13/D10/ADR-020）：主密钥存储（Keychain；A3 降级加密文件）在备份与
    // 供应商之间共享同一实例（keyring 自检一次）；加密缺省开启，密钥不可用时
    // 备份暂停并告警（不产出明文，ADR-020 缓解措施）。降级启动（无存储）不探测。
    let inner: std::sync::Arc<dyn ipc::IpcBackend> = match backup_deps {
        Some((reads, write)) => {
            let backend = std::sync::Arc::new(backup_control::BackupControlBackend::new(
                inner,
                data_dir.clone(),
                Some(reads),
                Some(write),
                handle.clone(),
                boot_secrets.clone(),
            ));
            if let (Some(orchestrator), Some(task)) =
                (&orchestrator, backend.spawn_auto_backup(&handle))
            {
                orchestrator.install_monitor("backup_auto", task);
            }
            backend as std::sync::Arc<dyn ipc::IpcBackend>
        }
        None => inner,
    };
    // M3-05（D11/D13）：诊断/容量装饰器——诊断包导出（脱敏 + 日志/健康/库摘要整合）、
    // `backup_list.reminder`（7 天未备份提醒）、已登记设置键读写。
    // M5-04/M5-05（D13/D4；ADR-021）：归档/保留装饰器——`session_export`/
    // `archive_export` 命令 + 保留清理任务（先归档后删除；后台调度器随核心接线，
    // 退出序列停止；`backup_list.retention` 与诊断包 `retention`/`export` 段）。
    let (reads_for_diagnostics, write_for_diagnostics) = match &diagnostics_deps {
        Some(deps) => (deps.reads.clone(), deps.write.clone()),
        None => (None, None),
    };
    let archive: std::sync::Arc<dyn ipc::IpcBackend> = {
        let backend = std::sync::Arc::new(archive_control::ArchiveControlBackend::new(
            inner,
            data_dir.clone(),
            reads_for_diagnostics,
            write_for_diagnostics,
            handle.clone(),
            std::sync::Arc::new(aether_control::SystemClock),
        ));
        if let (Some(orchestrator), Some(task)) = (&orchestrator, backend.spawn_retention(&handle))
        {
            orchestrator.install_monitor("retention", task);
        }
        backend as std::sync::Arc<dyn ipc::IpcBackend>
    };
    let inner = archive;
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
    // ADR-028 v0.9：供应商凭据预检（session_create fail-fast / 旧会话补选准备）。
    session_backend = session_backend.with_provider_env(provider_env_hook.clone());
    // M7-08/ADR-026 §2.1-1：`workspaces_list` 合成组「默认（数据目录）」的根路径展示。
    session_backend = session_backend.with_data_dir(data_dir.clone());
    // M3-08：工作区写路径接线（`workspace_set` 落库）与启动恢复（最近绑定工作区 →
    // 新会话记忆注入 + 权限基准同源；旧会话不迁移）。
    if let Some(write) = write_slot.clone() {
        session_backend = session_backend.with_workspace_store(write);
    }
    // M3-11（ADR-010 决策 3/D10）：模型与供应商配置命令面接线。密钥存储启动选择：
    // keyring 自检通过 → OS 凭据库；否则 A3 降级加密文件（口令环境钩子，测试/演练）；
    // 两者皆不可用 → 密钥字段命令回诊断错误（不落明文）。M5-03：与备份共用同一实例。
    // M7-09（ADR-026 §2.2）：Agent 预设命令面接线（`presets_list` / `preset_*`；
    // 校验/内置保护/审计语义在 `preset_control`，本层仅注入读写句柄）。
    if let (Some(reads), Some(write)) = (reads.clone(), write_slot.clone()) {
        session_backend = session_backend.with_presets(preset_control::PresetControl::new(
            reads,
            write,
            handle.clone(),
        ));
    }
    if let (Some(reads), Some(write)) = (reads, write_slot) {
        session_backend = session_backend.with_providers(provider_control::ProviderControl::new(
            reads,
            write,
            boot_secrets,
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
        pipeline: pipeline_for_bridge,
        write: write_for_bridge,
        keychain_preflight,
    }
}
