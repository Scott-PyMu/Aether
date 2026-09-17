//! M1-06 目录选择器抽象集成测试：路径 / 取消 / 错误 / 未接线 / 脚本化序列。
//!
//! 通过 `tauri::test` 的 MockRuntime 走真实命令注册路径（`startup_pick_target` →
//! `IpcState::picker()` → `DirectoryPicker::pick_directory`），断言注入替身的返回
//! 契约与结构化错误；真实系统选择器由 `scripts/test/m1-06/manual-picker-smoke.mjs`
//! 冒烟（人工/半自动，归档截图与日志）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aether_tauri::ipc::backend::IpcBackend;
use aether_tauri::ipc::dto::SessionListRequest;
use aether_tauri::ipc::error::IpcError;
use aether_tauri::ipc::{handler, IpcState};
use aether_tauri::picker::{DirectoryPicker, FixedDirectoryPicker};
use aether_tauri::startup::detect::{
    DetectionContext, NetworkDriveSource, PlatformKind, RegistrySource, ReparseSource,
};
use aether_tauri::startup::{DataDirSource, StartupGate};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};

/// Mock invoke 的本地源 URL：Tauri 自定义协议在 Windows 为 `http://tauri.localhost`，
/// 其他平台为 `tauri://localhost`；用错会被 ACL 判定为远端来源而拒绝（可移植性修复）。
#[cfg(windows)]
const INVOKE_URL: &str = "http://tauri.localhost";
#[cfg(not(windows))]
const INVOKE_URL: &str = "tauri://localhost";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "aether-m1-06-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn allow_context() -> DetectionContext {
    DetectionContext {
        platform: PlatformKind::Windows,
        home: None,
        env: std::collections::BTreeMap::new(),
        registry: RegistrySource::Unavailable,
        reparse: ReparseSource::Unavailable,
        network_drives: NetworkDriveSource::Unavailable,
    }
}

#[derive(Default)]
struct RecordingBackend {
    calls: Mutex<Vec<String>>,
}

impl RecordingBackend {
    fn calls(&self) -> Vec<String> {
        match self.calls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl IpcBackend for RecordingBackend {
    fn session_list(&self, _request: &SessionListRequest) -> Result<Value, IpcError> {
        match self.calls.lock() {
            Ok(mut guard) => guard.push("session_list".to_string()),
            Err(poisoned) => poisoned.into_inner().push("session_list".to_string()),
        }
        Ok(json!({ "recorded": "session_list" }))
    }
}

struct Fixture {
    #[allow(dead_code)]
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    backend: Arc<RecordingBackend>,
}

fn fixture(label: &str, picker: Option<Arc<dyn DirectoryPicker>>) -> Fixture {
    let root = temp_dir(label);
    let ready_dir = root.join("ready");
    std::fs::create_dir_all(&ready_dir).expect("创建就绪目录");
    let gate = Arc::new(StartupGate::bootstrap_at(
        ready_dir,
        DataDirSource::Default,
        allow_context(),
        None,
    ));
    let backend = Arc::new(RecordingBackend::default());
    let state = match picker {
        Some(picker) => {
            IpcState::with_startup_and_picker(backend.clone(), Vec::new(), gate, picker)
        }
        None => IpcState::with_startup(backend.clone(), Vec::new(), gate),
    };
    let app = mock_builder()
        .invoke_handler(handler())
        .manage(state)
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用");
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("创建 mock webview");
    Fixture {
        app,
        webview,
        backend,
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
        tauri::ipc::InvokeBody::Json(json!({ "payload": payload }))
    };
    let response = tauri::test::get_ipc_response(
        webview,
        InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: INVOKE_URL.parse().expect("URL"),
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

#[test]
fn startup_pick_target_returns_injected_path() {
    let picked = PathBuf::from(r"C:\Local\AetherData");
    let fixture = fixture(
        "picker-path",
        Some(Arc::new(FixedDirectoryPicker::with_path(picked.clone()))),
    );
    let value =
        invoke(&fixture.webview, "startup_pick_target", Value::Null).expect("选择器注入路径应成功");
    assert_eq!(
        value["target_dir"].as_str(),
        Some(picked.to_string_lossy().as_ref())
    );
    assert!(
        fixture.backend.calls().is_empty(),
        "选择器命令不得触碰业务后端"
    );
}

#[test]
fn startup_pick_target_returns_null_on_cancel() {
    let fixture = fixture(
        "picker-cancel",
        Some(Arc::new(FixedDirectoryPicker::with_cancel())),
    );
    let value = invoke(&fixture.webview, "startup_pick_target", Value::Null)
        .expect("取消应返回成功 + null");
    assert_eq!(value["target_dir"], Value::Null);
}

/// ADR-006 决策 5（v0.2 修订）：严格无参命令——未知成员必须结构化拒绝，
/// 合法无参调用不受影响。
#[test]
fn startup_pick_target_rejects_unknown_payload_members() {
    let fixture = fixture(
        "picker-strict",
        Some(Arc::new(FixedDirectoryPicker::with_cancel())),
    );
    let error = invoke(
        &fixture.webview,
        "startup_pick_target",
        json!({ "extra": true }),
    )
    .expect_err("严格无参命令必须拒绝 unknown 成员");
    assert_eq!(error["code"], "unknown_field", "{error}");
    assert_eq!(error["field"], "extra", "{error}");

    let value = invoke(&fixture.webview, "startup_pick_target", Value::Null)
        .expect("无参调用应成功");
    assert_eq!(value["target_dir"], Value::Null);
}

#[test]
fn startup_pick_target_maps_picker_error_to_structured_error() {
    let fixture = fixture(
        "picker-error",
        Some(Arc::new(FixedDirectoryPicker::with_error("对话框不可用"))),
    );
    let error = invoke(&fixture.webview, "startup_pick_target", Value::Null)
        .expect_err("选择器错误必须结构化返回");
    assert_eq!(error["code"], "internal", "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("对话框不可用")),
        "{error}"
    );
}

#[test]
fn startup_pick_target_without_picker_is_not_implemented() {
    let fixture = fixture("picker-absent", None);
    let error = invoke(&fixture.webview, "startup_pick_target", Value::Null)
        .expect_err("未接线选择器必须拒绝");
    assert_eq!(error["code"], "not_implemented", "{error}");
}

#[test]
fn scripted_picker_consumes_queue_then_repeats_last() {
    let first = PathBuf::from(r"C:\First");
    let picker = Arc::new(
        FixedDirectoryPicker::new()
            .then_path(first.clone())
            .then_cancel(),
    );
    let fixture = fixture("picker-script", Some(picker));

    let one = invoke(&fixture.webview, "startup_pick_target", Value::Null).expect("第一次");
    assert_eq!(one["target_dir"].as_str(), Some("C:\\First"));
    let two = invoke(&fixture.webview, "startup_pick_target", Value::Null).expect("第二次");
    assert_eq!(two["target_dir"], Value::Null);
    // 队列耗尽后重复最后一项（取消）。
    let three = invoke(&fixture.webview, "startup_pick_target", Value::Null).expect("第三次");
    assert_eq!(three["target_dir"], Value::Null);
}
