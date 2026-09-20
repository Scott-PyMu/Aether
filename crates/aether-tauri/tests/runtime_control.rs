//! M2-01 DoD6：`runtime_retry` / `runtime_enable` IPC 接线（M1-10 服务契约 §6 薄适配）。
//!
//! 覆盖：
//! - 命令级严格解析回归（未知字段 → `unknown_field`；非法 runtime_id → `invalid_format`）；
//! - 错误映射冻结：白名单不命中 → `invalid_enum`；状态不允许 → `invalid_value`；
//! - 状态转移完成：`disabled + start_failed` 上 `runtime_retry` 触发
//!   `disabled → cold → starting → disabled(start_failed)` 并回执 `failed`；
//!   `disabled` 上 `runtime_enable` 允许（契约 §6.4）；
//! - `untrusted` 拒绝启用 → `invalid_value`（`NeedsRemedy`）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use aether_adapters::protocol::DisabledReason;
use aether_adapters::supervisor::{Supervisor, SupervisorConfig};
use aether_core::RuntimeStatus;
use aether_tauri::ipc::backend::NotImplementedBackend;
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::ipc::{handler, IpcBackend, IpcState};
use aether_tauri::runtime_control::{
    boot_supervisor, map_supervisor_error, mock_spec, RuntimeControlBackend, SupervisorControl,
};
use serde_json::Value;
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

struct Fixture {
    #[allow(dead_code)]
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    #[allow(dead_code)]
    dir: TempDir,
}

fn fixture_with_backend(dir: TempDir, backend: Arc<dyn IpcBackend>) -> Fixture {
    let app = mock_builder()
        .invoke_handler(handler())
        .manage(IpcState::new(backend, Vec::new()))
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用");
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("创建 mock webview");
    Fixture { app, webview, dir }
}

fn invoke(
    webview: &WebviewWindow<MockRuntime>,
    command: &str,
    payload: Value,
) -> Result<Value, Value> {
    let body = if payload.is_null() {
        tauri::ipc::InvokeBody::default()
    } else {
        tauri::ipc::InvokeBody::Json(serde_json::json!({ "payload": payload }))
    };
    let response = tauri::test::get_ipc_response(
        webview,
        InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().expect("URL"),
            body,
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    match response {
        Ok(body) => Ok(body.deserialize::<Value>().unwrap_or(Value::Null)),
        Err(value) => Err(value),
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn error_code(value: &Value) -> String {
    value["code"].as_str().unwrap_or_default().to_owned()
}

/// 组装「缺二进制」监督器：启动必然 `disabled + start_failed`，用于状态转移与错误映射。
fn supervisor_with_missing_binary(
    dir: &TempDir,
    handle: &tokio::runtime::Handle,
) -> (Arc<Supervisor>, Arc<SupervisorControl>) {
    let missing = dir.path().join("definitely-missing-adapter-binary");
    let spec = mock_spec("mock", &missing.to_string_lossy());
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("构造监督器"),
    );
    let control = Arc::new(SupervisorControl::new(
        Arc::clone(&supervisor),
        handle.clone(),
        Duration::from_secs(10),
    ));
    (supervisor, control)
}

#[test]
fn runtime_retry_and_enable_are_wired_and_transition_states() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let (supervisor, control) = supervisor_with_missing_binary(&dir, runtime.handle());
    let backend: Arc<dyn IpcBackend> = Arc::new(RuntimeControlBackend::new(
        Arc::new(NotImplementedBackend),
        Some(control.clone()),
    ));
    let fixture = fixture_with_backend(dir, backend);

    // 1) 白名单不命中 → invalid_enum（冻结映射）。
    let error = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "nope"}),
    )
    .expect_err("未知 runtime 必须失败");
    assert_eq!(error_code(&error), "invalid_enum", "{error}");

    // 2) cold 状态 retry → invalid_value（RetryNotAllowed）。
    let error = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect_err("cold 上 retry 必须被拒");
    assert_eq!(error_code(&error), "invalid_value", "{error}");

    // 3) 触发一次启动失败 → disabled + start_failed。
    let supervisor_handle = supervisor.get("mock").expect("注册表命中");
    let outcome = runtime.block_on(supervisor_handle.start());
    println!("[m2-01-ipc] 首次启动 outcome={outcome:?}");
    let status = runtime.block_on(supervisor.get("mock").unwrap().status());
    let reason = runtime.block_on(supervisor.get("mock").unwrap().status_reason());
    assert_eq!(status, RuntimeStatus::Disabled);
    assert_eq!(reason, Some(DisabledReason::StartFailed));

    // 4) disabled + start_failed 上 retry：命令成功返回回执，且完成状态转移
    //    （disabled → cold → starting → disabled(start_failed)）。
    let report = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect("disabled + start_failed 上 retry 必须受理");
    println!("[m2-01-ipc] runtime_retry report={report}");
    assert_eq!(report["runtime_id"], "mock");
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["status"], "disabled");
    assert_eq!(report["status_reason"], "start_failed");
    assert!(
        report["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("start_failed")),
        "detail 必须携带原因: {report}"
    );

    // 5) disabled 上 enable：允许（crash_loop/start_failed 等非 untrusted/version_mismatch）。
    let report = invoke(
        &fixture.webview,
        "runtime_enable",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect("disabled 上 enable 必须受理");
    println!("[m2-01-ipc] runtime_enable report={report}");
    assert_eq!(report["outcome"], "failed");
}

/// `untrusted` 状态必须先修复清单/版本：enable / retry 均拒绝（invalid_value）。
#[test]
fn untrusted_requires_remedy_before_enable() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let (supervisor, control) = supervisor_with_missing_binary(&dir, runtime.handle());
    let backend: Arc<dyn IpcBackend> = Arc::new(RuntimeControlBackend::new(
        Arc::new(NotImplementedBackend),
        Some(control),
    ));
    let fixture = fixture_with_backend(dir, backend);

    // cold → disabled + untrusted（准入拒绝路径，评审 #1）。
    let rejected = runtime.block_on(
        supervisor
            .get("mock")
            .unwrap()
            .reject_untrusted("第三方 manifest".to_owned()),
    );
    println!("[m2-01-ipc] reject_untrusted outcome={rejected:?}");
    assert_eq!(
        runtime.block_on(supervisor.get("mock").unwrap().status_reason()),
        Some(DisabledReason::Untrusted)
    );

    let error = invoke(
        &fixture.webview,
        "runtime_enable",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect_err("untrusted 直接 enable 必须被拒");
    assert_eq!(error_code(&error), "invalid_value", "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("untrusted")),
        "消息必须携带原因: {error}"
    );

    let error = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect_err("untrusted 上 retry 必须被拒（仅 disabled + start_failed）");
    assert_eq!(error_code(&error), "invalid_value", "{error}");
}

/// M1-08 校验矩阵回归：DTO 错误在进入后端前被拦截（不触发监督器）。
#[test]
fn runtime_control_commands_keep_m1_08_validation_regression() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let (supervisor, control) = supervisor_with_missing_binary(&dir, runtime.handle());
    let backend: Arc<dyn IpcBackend> = Arc::new(RuntimeControlBackend::new(
        Arc::new(NotImplementedBackend),
        Some(control),
    ));
    let fixture = fixture_with_backend(dir, backend);

    // 未知字段 → unknown_field。
    let error = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "mock", "extra": true}),
    )
    .expect_err("未知字段必须拒绝");
    assert_eq!(error_code(&error), "unknown_field", "{error}");

    // 非法标识符 → invalid_format。
    let error = invoke(
        &fixture.webview,
        "runtime_enable",
        serde_json::json!({"runtime_id": "bad id!"}),
    )
    .expect_err("非法 runtime_id 必须拒绝");
    assert_eq!(error_code(&error), "invalid_format", "{error}");

    // 缺少字段 → missing_field。
    let error = invoke(&fixture.webview, "runtime_retry", serde_json::json!({}))
        .expect_err("缺少字段必须拒绝");
    assert_eq!(error_code(&error), "missing_field", "{error}");

    // 校验失败不得触碰监督器状态（仍为 cold）。
    let status = runtime.block_on(supervisor.get("mock").unwrap().status());
    assert_eq!(status, RuntimeStatus::Cold);
}

/// 未接线（监督器台账初始化失败）时回 `core_not_ready`，health 不受影响。
#[test]
fn runtime_control_returns_core_not_ready_when_unwired() {
    let dir = TempDir::new().expect("临时目录");
    let backend: Arc<dyn IpcBackend> = Arc::new(RuntimeControlBackend::new(
        Arc::new(NotImplementedBackend),
        None,
    ));
    let fixture = fixture_with_backend(dir, backend);
    let error = invoke(
        &fixture.webview,
        "runtime_retry",
        serde_json::json!({"runtime_id": "mock"}),
    )
    .expect_err("未接线必须回 core_not_ready");
    assert_eq!(error_code(&error), "core_not_ready", "{error}");
}

/// 契约一致性静态断言：错误码映射表与 M1-10 §6.3 对齐（不新增码）。
#[test]
fn error_mapping_matches_frozen_contract() {
    let cases = [
        (
            aether_adapters::supervisor::SupervisorError::UnknownRuntime("x".to_owned()),
            IpcErrorCode::InvalidEnum,
        ),
        (
            aether_adapters::supervisor::SupervisorError::RetryNotAllowed {
                status: RuntimeStatus::Ready,
                reason: None,
            },
            IpcErrorCode::InvalidValue,
        ),
        (
            aether_adapters::supervisor::SupervisorError::EnableNotAllowed {
                status: RuntimeStatus::Ready,
            },
            IpcErrorCode::InvalidValue,
        ),
        (
            aether_adapters::supervisor::SupervisorError::NeedsRemedy {
                reason: DisabledReason::Untrusted,
            },
            IpcErrorCode::InvalidValue,
        ),
        (
            aether_adapters::supervisor::SupervisorError::Transition("x".to_owned()),
            IpcErrorCode::Internal,
        ),
        (
            aether_adapters::supervisor::SupervisorError::Ledger("x".to_owned()),
            IpcErrorCode::Internal,
        ),
    ];
    for (error, expected) in cases {
        let mapped = map_supervisor_error(error);
        assert_eq!(mapped.code, expected);
    }
    // 配置基线：initialize 超时 10s（D6 方法表）。
    assert_eq!(
        SupervisorConfig::d5().initialize_timeout,
        Duration::from_secs(10)
    );
}
