//! ADR-007 增量修订 1 决策 1：`health` 命令**真实接线**测试；增量修订 2 追加
//! `core_not_ready`（T12 顺序）与 `runtimes` 的 `null`/`[]` 语义。
//!
//! 覆盖：
//! - 真实 `EventPipeline`（临时 SQLite + 单写队列 + 读连接池）→ `health` 返回
//!   `storage_state=normal`，`write_queue_depth`/`runtimes`/`ts` 齐备；
//! - `signal_degraded` 后同一命令返回 `persist_degraded` + 触发源（D4 唯一事实源）；
//! - health 为只读查询：连续调用不落库、不产生事件；
//! - 启动失败（损坏库 → 安全模式）→ `degraded_backend` 返回只读降级报告，
//!   不回退 `not_implemented`；
//! - **T12 顺序（增量 2）**：Builder 阶段延迟后端（窗口加载期）→ `health` 返回
//!   `core_not_ready`，门命令 `startup_get` 仍可达；`setup` 注入后端后恢复正常；
//! - `runtimes`：未接线 → `null`；已接线 → 数组（含空数组与条目透传）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use aether_control::{
    DegradeTrigger, EventPipeline, PipelineConfig, StartupSelfCheckReport, StoreEventSource,
    StoreJournal,
};
use aether_store::{StoreRuntime, WriteQueueConfig};
use aether_tauri::core_health::{
    boot_core_health, degraded_backend, CoreHealthBackend, HealthProvider, RuntimeSummary,
    StaticHealthSource, StaticRuntimeSummaries, StorageHealthSnapshot,
};
use aether_tauri::ipc::{handler, IpcBackend, IpcState};
use aether_tauri::startup::detect::{
    DetectionContext, NetworkDriveSource, PlatformKind, RegistrySource, ReparseSource,
};
use aether_tauri::startup::{DataDirSource, StartupGate};
use serde_json::Value;
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, Manager, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

struct Fixture {
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    #[allow(dead_code)]
    dir: TempDir,
}

fn fixture_with_backend(dir: TempDir, backend: Arc<dyn IpcBackend>) -> Fixture {
    fixture_with_state(dir, IpcState::new(backend, Vec::new()))
}

fn fixture_with_state(dir: TempDir, state: IpcState) -> Fixture {
    let app = mock_builder()
        .invoke_handler(handler())
        .manage(state)
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用");
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("创建 mock webview");
    Fixture { app, webview, dir }
}

/// 允许上下文（无任何同步盘命中）→ 启动门 Ready。
fn allow_context() -> DetectionContext {
    DetectionContext {
        platform: PlatformKind::Windows,
        home: None,
        env: BTreeMap::new(),
        registry: RegistrySource::Unavailable,
        reparse: ReparseSource::Unavailable,
        network_drives: NetworkDriveSource::Unavailable,
    }
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

/// 真实管线 → `health` 两态返回；health 为只读（不落库、不产生事件）。
#[test]
fn health_returns_real_normal_then_persist_degraded() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("打开存储");
    let journal = Arc::new(StoreJournal::new(storage.queue().clone()));
    let source = Arc::new(StoreEventSource::new(storage.reads().clone()));
    let pipeline = EventPipeline::start(
        PipelineConfig::default(),
        journal,
        source,
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &handle,
    )
    .expect("启动管线");
    let backend: Arc<dyn IpcBackend> = Arc::new(CoreHealthBackend::from_pipeline(
        pipeline.clone(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        storage,
    ));
    let fixture = fixture_with_backend(dir, backend);

    // 正常态。
    let value = invoke(&fixture.webview, "health", Value::Null).expect("health 必须成功");
    println!("health(normal) = {value}");
    assert_eq!(value["storage_state"], "normal");
    assert_eq!(value["write_queue_depth"], 0);
    assert!(
        value.get("runtimes").is_some(),
        "runtimes 键必须存在（未接线为 null）: {value}"
    );
    assert!(
        value["runtimes"].is_null(),
        "监督器未接线 → null（区分已接线空数组）: {value}"
    );
    assert!(
        value["ts"].as_i64().is_some_and(|ts| ts > 0),
        "ts 必须为毫秒时间戳: {value}"
    );
    assert!(
        value.get("degrade_trigger").is_none(),
        "正常态不带降级字段: {value}"
    );
    assert!(value.get("detail").is_none(), "正常态不带 detail: {value}");

    // 降级态（D4 事实源：EventPipeline::health()）。
    runtime
        .block_on(pipeline.signal_degraded(DegradeTrigger::SpaceGuard { free_bytes: 1 }))
        .expect("降级信号");
    let value = invoke(&fixture.webview, "health", Value::Null).expect("降级期 health 仍可达");
    println!("health(persist_degraded) = {value}");
    assert_eq!(value["storage_state"], "persist_degraded");
    assert_eq!(value["degrade_trigger"], "space_guard");
    assert!(
        value["degraded_since_ms"].as_i64().is_some(),
        "降级需带进入时间: {value}"
    );

    // 只读查询：两次调用均不落库、不产生事件。
    let health = pipeline.health();
    assert_eq!(health.persisted_events, 0, "health 不得写事件");
    assert_eq!(health.broadcast_events, 0, "health 不得广播事件");
}

/// 启动失败（损坏库 → 安全模式）→ 只读降级报告（非 `not_implemented`）。
#[test]
fn health_reports_degraded_when_core_boot_fails() {
    let dir = TempDir::new().expect("临时目录");
    std::fs::write(dir.path().join("aether.db"), b"not a database").expect("写入损坏库");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    let backend: Arc<dyn IpcBackend> = match boot_core_health(dir.path(), &handle) {
        Ok(_) => panic!("损坏库不得启动核心"),
        Err(error) => {
            assert!(
                !error.to_string().is_empty(),
                "启动失败必须带原因: {error:?}"
            );
            Arc::new(degraded_backend(&error.to_string()))
        }
    };
    let fixture = fixture_with_backend(dir, backend);

    let value = invoke(&fixture.webview, "health", Value::Null).expect("health 必须成功");
    println!("health(boot-failed) = {value}");
    assert_eq!(value["storage_state"], "persist_degraded");
    assert_eq!(value["degrade_trigger"], "integrity_failure");
    assert!(
        value["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()),
        "降级需带原因 detail: {value}"
    );
    assert!(
        value["degraded_since_ms"].as_i64().is_some(),
        "降级需带进入时间: {value}"
    );
}

/// ADR-007 增量 2 / T12 顺序：Builder 阶段延迟后端（窗口加载期）→ `health`
/// 返回 `core_not_ready`；门命令 `startup_get` 不依赖后端仍可达；
/// `setup` 注入真实后端后恢复正常。
#[test]
fn core_not_ready_until_backend_installed() {
    let dir = TempDir::new().expect("临时目录");
    let gate = StartupGate::bootstrap_at(
        dir.path().to_path_buf(),
        DataDirSource::Default,
        allow_context(),
        None,
    );
    let state = IpcState::with_startup_deferred(Vec::new(), Arc::new(gate));
    let fixture = fixture_with_state(dir, state);

    // 后端未注入：业务命令返回结构化 `core_not_ready`（非 not_implemented、非 panic）。
    let error = invoke(&fixture.webview, "health", Value::Null).expect_err("后端未注入必须报错");
    println!("health(core_not_ready) = {error}");
    assert_eq!(error["code"], "core_not_ready");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("未就绪")),
        "需说明启动序列未完成: {error}"
    );

    // 窗口加载期门命令可达（延迟后端的目的：startup_* 不依赖后端）。
    let snapshot =
        invoke(&fixture.webview, "startup_get", Value::Null).expect("门命令在延迟后端期必须可达");
    assert_eq!(snapshot["phase"], "ready");

    // 模拟 setup 注入真实后端（等价 run() 的 install_backend）。
    let provider = HealthProvider::new(
        Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        })),
        Arc::new(StaticRuntimeSummaries::unwired()),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(CoreHealthBackend::new(provider, None));
    assert!(
        fixture
            .app
            .state::<IpcState>()
            .install_backend(Arc::clone(&backend)),
        "首次注入必须成功"
    );
    assert!(
        !fixture.app.state::<IpcState>().install_backend(backend),
        "重复注入必须被拒绝（OnceLock 单次语义）"
    );

    let value = invoke(&fixture.webview, "health", Value::Null).expect("注入后必须成功");
    assert_eq!(value["storage_state"], "normal");
    assert!(value["runtimes"].is_null());
}

/// ADR-007 增量 2：监督器已接线时 `runtimes` 为数组（含条目透传）。
#[test]
fn health_passes_through_wired_runtime_summaries() {
    let provider = HealthProvider::new(
        Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        })),
        Arc::new(StaticRuntimeSummaries::wired(vec![RuntimeSummary {
            id: "mock".to_owned(),
            status: "ready".to_owned(),
            status_reason: None,
        }])),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(CoreHealthBackend::new(provider, None));
    let fixture = fixture_with_backend(TempDir::new().expect("临时目录"), backend);

    let value = invoke(&fixture.webview, "health", Value::Null).expect("health 必须成功");
    println!("health(wired-runtimes) = {value}");
    assert_eq!(value["runtimes"][0]["id"], "mock", "{value}");
    assert_eq!(value["runtimes"][0]["status"], "ready", "{value}");
    assert!(
        value["runtimes"][0].get("status_reason").is_none(),
        "空 reason 不序列化: {value}"
    );
}
