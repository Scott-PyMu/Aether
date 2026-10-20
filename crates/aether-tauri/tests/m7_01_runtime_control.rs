//! M7-01 DoD5/DoD7：`runtime_disable` / `runtime_enable` 命令层流程
//! （偏好写序、在途确认门、审计、错误映射；ADR-015 决策 3/5/6）。
//!
//! 说明：在途 run 的收口（活跃/queued）由 `m7_01_hotplug.rs` 用真实夹具端到端覆盖；
//! 本文件聚焦命令层语义（不启动真实适配器进程，用「缺二进制」监督器构造确定状态）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_core::{Runtime, RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus, TokenUsage};
use aether_store::StoreCommand;
use aether_tauri::core_health::{boot_core_full, StaticRuntimeSummaries};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::runtime_control::boot_supervisor;
use aether_tauri::runtime_control::{
    mock_spec, run_supervisor_startup_with_prefs, RuntimeControl, SupervisorControl,
    RUNTIME_CONTROL_TIMEOUT,
};
use tempfile::TempDir;

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn count_action(runtime: &tokio::runtime::Runtime, reads: &aether_store::ReadPool, action: &str) -> u64 {
    let action = action.to_owned();
    runtime
        .block_on(reads.with_connection(move |connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM audit_log WHERE action = ?1",
                    [action.as_str()],
                    |row| row.get(0),
                )
                .map_err(aether_store::StoreError::from)?;
            Ok(u64::try_from(count).unwrap_or(0))
        }))
        .unwrap_or(0)
}

fn runtime_row(id: &str) -> Runtime {
    Runtime {
        id: RuntimeId::new(id).unwrap(),
        name: id.to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec![],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Cold,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn session_row(id: &str) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "t".to_owned(),
        status: SessionStatus::Running,
        model: None,
        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
        permission_mode: aether_core::PermissionMode::default(),
        preset_id: None,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

/// DoD5/DoD7：`runtime_disable`（无在途）→ 偏好先写 → `disabled + user_disabled`
/// 回执；`runtime_enable` 偏好先写、启动失败按既有收口；`disabled` 上再禁用拒绝；
/// 未知 id → `invalid_enum`；未接线 → `core_not_ready`。
#[test]
fn disable_and_enable_command_flow_persists_preference_and_audits() {
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let boot = boot_core_full(
        &data_dir,
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心启动");

    // 缺二进制监督器：cold 上禁用合法（无进程可终止）；enable 必然 start_failed。
    let missing = dir.path().join("definitely-missing-adapter-binary");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", &missing.to_string_lossy())],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器"),
    );
    let control = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    )
    .with_runtime_store(boot.reads.clone(), boot.write.clone());

    // ===== disable（cold，无在途）：成功，outcome=disabled / reason=user_disabled =====
    let report = control.disable("mock", false).expect("cold 上禁用");
    assert_eq!(report["runtime_id"], "mock");
    assert_eq!(report["outcome"], "disabled");
    assert_eq!(report["status"], "disabled");
    assert_eq!(report["status_reason"], "user_disabled");
    assert!(
        report["detail"].as_str().unwrap_or("").contains("在途计数=0"),
        "{report}"
    );
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(preferences.get("mock"), Some(&false), "偏好必须先写 0");
    assert_eq!(count_action(&runtime, &boot.reads, "runtime.disable"), 1, "禁用审计");

    // disabled 上再 disable → invalid_value（命令矩阵）。
    let error = control.disable("mock", false).expect_err("disabled 上再禁用必须拒绝");
    assert_eq!(error.code, IpcErrorCode::InvalidValue);

    // 未知 runtime_id → invalid_enum（不影响偏好）。
    let error = control.disable("nope", false).expect_err("未知 id 必须拒绝");
    assert_eq!(error.code, IpcErrorCode::InvalidEnum);

    // ===== enable（user_disabled）：偏好先写 1；启动失败按既有 disabled+reason 收口 =====
    let report = control.enable("mock").expect("user_disabled 上 enable 必须受理");
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["status"], "disabled");
    assert_eq!(report["status_reason"], "start_failed");
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(
        preferences.get("mock"),
        Some(&true),
        "启用前偏好必须先写 1（§2.1#5）"
    );
    assert_eq!(count_action(&runtime, &boot.reads, "runtime.enable"), 1, "启用审计");

    // start_failed（status=disabled）上 disable → invalid_value（D-2 矩阵：disabled 上不转移）。
    let error = control
        .disable("mock", true)
        .expect_err("disabled（start_failed）上禁用必须拒绝");
    assert_eq!(error.code, IpcErrorCode::InvalidValue);

    // ===== 未接线（无存储）：disable 回 core_not_ready；enable 沿用既有语义 =====
    let unwired = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    );
    let error = unwired.disable("mock", false).expect_err("未接线必须 core_not_ready");
    assert_eq!(error.code, IpcErrorCode::CoreNotReady);

    runtime.block_on(boot.pipeline.shutdown()).expect("管线关闭");
    runtime.block_on(supervisor.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = runtime.block_on(storage.shutdown());
    }
}

/// DoD5 回归：`untrusted` 不可直接启用（`NeedsRemedy` → `invalid_value`），
/// 且拒绝路径不写偏好（不产生「先开后关」痕迹）。
#[test]
fn enable_on_untrusted_is_rejected_without_preference_write() {
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let boot = boot_core_full(
        &data_dir,
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心启动");
    let missing = dir.path().join("definitely-missing-adapter-binary");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", &missing.to_string_lossy())],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器"),
    );
    let rejected = runtime.block_on(
        supervisor
            .get("mock")
            .unwrap()
            .reject_untrusted("第三方 manifest".to_owned()),
    );
    assert!(matches!(
        rejected,
        aether_adapters::supervisor::StartOutcome::Rejected { .. }
    ));
    let control = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    )
    .with_runtime_store(boot.reads.clone(), boot.write.clone());
    let error = control.enable("mock").expect_err("untrusted 不可直接启用");
    assert_eq!(error.code, IpcErrorCode::InvalidValue);
    assert!(error.message.contains("untrusted"), "{}", error.message);
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(preferences.get("mock"), None, "拒绝路径不得写偏好");

    runtime.block_on(boot.pipeline.shutdown()).expect("管线关闭");
    runtime.block_on(supervisor.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = runtime.block_on(storage.shutdown());
    }
}

/// DoD2（命令层确认门）：在途计数来自存储读路径（会话 running + queued run），
/// 未确认 → `runtime_busy` 且**不写偏好、不转移**；确认后成功。
#[test]
fn runtime_busy_gate_blocks_unconfirmed_disable_without_side_effects() {
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let boot = boot_core_full(
        &data_dir,
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心启动");

    // 播种：runtime 行（偏好缺省 1）+ running 会话（在途计数 1）。
    runtime
        .block_on(
            boot.write
                .execute(StoreCommand::EnsureRuntime {
                    runtime: runtime_row("mock"),
                }),
        )
        .expect("EnsureRuntime");
    runtime
        .block_on(
            boot.write
                .execute(StoreCommand::InsertSession {
                    session: session_row("01J8ZQ5R0N7W9Y8X6V4T2S0K31"),
                }),
        )
        .expect("InsertSession");

    let missing = dir.path().join("definitely-missing-adapter-binary");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", &missing.to_string_lossy())],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器"),
    );
    let control = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    )
    .with_runtime_store(boot.reads.clone(), boot.write.clone());

    let error = control.disable("mock", false).expect_err("在途未确认必须拒绝");
    assert_eq!(error.code, IpcErrorCode::RuntimeBusy);
    assert_eq!(error.field.as_deref(), Some("confirm_terminate"));
    assert!(error.message.contains("1 个在途"), "{}", error.message);
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(preferences.get("mock"), Some(&true), "未确认不得写偏好");
    assert_eq!(
        runtime.block_on(supervisor.get("mock").unwrap().status()),
        RuntimeStatus::Cold,
        "未确认不得转移"
    );

    let report = control.disable("mock", true).expect("确认后禁用");
    assert_eq!(report["outcome"], "disabled");
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(preferences.get("mock"), Some(&false));
    // 会话行不因禁用被改写（收口由执行器按既有路径承担）。
    let session = runtime
        .block_on(boot.reads.session(&SessionId::new("01J8ZQ5R0N7W9Y8X6V4T2S0K31").unwrap()))
        .expect("读会话")
        .expect("会话行");
    assert_eq!(session.status, SessionStatus::Running);

    runtime.block_on(boot.pipeline.shutdown()).expect("管线关闭");
    runtime.block_on(supervisor.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = runtime.block_on(storage.shutdown());
    }
}

/// DoD7 回归：启动矩阵经 `run_supervisor_startup_with_prefs` 读取偏好——
/// `enabled=0` 条目不预热、`runtimes_list` 的 `enabled` 与偏好一致（读路径同源）。
#[test]
fn startup_matrix_preference_round_trips_through_runtime_preferences() {
    let dir = TempDir::new().expect("临时目录");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("数据目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let boot = boot_core_full(
        &data_dir,
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心启动");

    let missing = dir.path().join("definitely-missing-adapter-binary");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", &missing.to_string_lossy())],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器"),
    );
    let control = SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        RUNTIME_CONTROL_TIMEOUT,
    )
    .with_runtime_store(boot.reads.clone(), boot.write.clone());
    // 禁用写偏好（row 不存在 → upsert 完整行）。
    let _ = control.disable("mock", false).expect("禁用");
    let preferences = runtime
        .block_on(boot.reads.runtime_preferences())
        .expect("偏好读");
    assert_eq!(preferences.get("mock"), Some(&false));

    // 重新构造监督器（模拟重启）并按偏好启动：enabled=0 不预热。
    let supervisor2 = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", &missing.to_string_lossy())],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("监督器2"),
    );
    let startup = run_supervisor_startup_with_prefs(&supervisor2, &handle, &preferences)
        .expect("启动序列尾段");
    assert!(matches!(
        startup.warmups[0].1,
        aether_adapters::supervisor::StartOutcome::Disabled { .. }
    ));
    assert_eq!(
        runtime.block_on(supervisor2.get("mock").unwrap().status()),
        RuntimeStatus::Disabled
    );
    assert_eq!(
        runtime
            .block_on(supervisor2.get("mock").unwrap().status_reason())
            .map(|reason| reason.as_str()),
        Some("user_disabled")
    );
    assert!(!runtime.block_on(supervisor2.get("mock").unwrap().is_running()));

    runtime.block_on(boot.pipeline.shutdown()).expect("管线关闭");
    runtime.block_on(supervisor.shutdown_all());
    runtime.block_on(supervisor2.shutdown_all());
    if let Some(storage) = boot.storage.take() {
        let _ = runtime.block_on(storage.shutdown());
    }
}
