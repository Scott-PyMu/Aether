//! M3-09 DoD4：`ref_pick` 命令层（MockRuntime + 注入选择器替身）。
//!
//! - `{ kind }` 严格解析：未知 kind → `invalid_enum`、未知成员 → `unknown_field`；
//! - 文件/目录选择走 `DirectoryPicker` 抽象（E2E 注入替身），取消 → `{ path: null }`；
//! - 选择器错误 → `internal`；未接线 → `not_implemented`；
//! - 选择器命令不触碰业务后端；`artifact_add` 的路径拒绝发生在命令层
//!   （canonicalize 失败 → `artifact_path_rejected`，后端不被调用）。
//!
//! 真实系统选择器由人工冒烟（`scripts/test/m1-06/manual-picker-smoke.mjs` 先例；
//! `ref_pick` 复用同一选择器抽象与同一 Tauri dialog 调用面）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
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

/// 证据归档（`AETHER_M3_09_EVIDENCE_DIR`；供 Gate 3 逐条出示）。
fn evidence(name: &str, value: &Value) {
    println!("[m3-09] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_09_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{name}.json")), text);
}

/// Mock invoke 的本地源 URL（与 M1-06 先例一致；Windows 为 `http://tauri.localhost`）。
#[cfg(windows)]
const INVOKE_URL: &str = "http://tauri.localhost";
#[cfg(not(windows))]
const INVOKE_URL: &str = "tauri://localhost";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "aether-m3-09-{label}-{}-{}",
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

fn build_fixture(label: &str, picker: Option<Arc<dyn DirectoryPicker>>) -> Fixture {
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
    let body = tauri::ipc::InvokeBody::Json(json!({ "payload": payload }));
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

/// DoD4：`kind=file` / `kind=directory` 分别走文件与目录选择；取消返回 `path: null`。
#[test]
fn ref_pick_dispatches_kind_and_returns_injected_path_or_null() {
    let file = PathBuf::from(r"C:\Workspace\notes.md");
    let folder = PathBuf::from(r"C:\Workspace\docs");
    let picker =
        Arc::new(FixedDirectoryPicker::with_file_path(file.clone()).then_path(folder.clone()));
    let fixture = build_fixture("ref-pick-path", Some(picker));

    let picked_file =
        invoke(&fixture.webview, "ref_pick", json!({ "kind": "file" })).expect("文件选择应成功");
    assert_eq!(
        picked_file["path"].as_str(),
        Some(file.to_string_lossy().as_ref())
    );
    let picked_dir = invoke(&fixture.webview, "ref_pick", json!({ "kind": "directory" }))
        .expect("目录选择应成功");
    assert_eq!(
        picked_dir["path"].as_str(),
        Some(folder.to_string_lossy().as_ref())
    );
    assert!(
        fixture.backend.calls().is_empty(),
        "选择器命令不得触碰业务后端"
    );

    // 取消（文件队列长度 1 → 重复返回同一项；此处换用专门取消夹具）。
    let cancel = build_fixture(
        "ref-pick-cancel",
        Some(Arc::new(FixedDirectoryPicker::with_file_cancel())),
    );
    let value = invoke(&cancel.webview, "ref_pick", json!({ "kind": "file" }))
        .expect("取消应返回成功 + null");
    assert_eq!(value["path"], Value::Null);

    evidence(
        "dod4_ref_pick",
        &json!({
            "file": picked_file["path"],
            "directory": picked_dir["path"],
            "cancel": value["path"],
            "backend_untouched": fixture.backend.calls().is_empty(),
        }),
    );
}

/// DoD4：严格解析——未知 kind → `invalid_enum`；未知成员 → `unknown_field`；缺成员 → `missing_field`。
#[test]
fn ref_pick_rejects_invalid_payload_strictly() {
    let fixture = build_fixture(
        "ref-pick-strict",
        Some(Arc::new(FixedDirectoryPicker::with_file_cancel())),
    );

    let error = invoke(&fixture.webview, "ref_pick", json!({ "kind": "binary" }))
        .expect_err("未知 kind 必须拒绝");
    assert_eq!(error["code"], "invalid_enum", "{error}");

    let error = invoke(
        &fixture.webview,
        "ref_pick",
        json!({ "kind": "file", "extra": true }),
    )
    .expect_err("未知成员必须拒绝");
    assert_eq!(error["code"], "unknown_field", "{error}");
    assert_eq!(error["field"], "extra", "{error}");

    let error = invoke(&fixture.webview, "ref_pick", json!({})).expect_err("缺 kind 必须拒绝");
    assert_eq!(error["code"], "missing_field", "{error}");

    assert!(
        fixture.backend.calls().is_empty(),
        "校验失败不得触碰业务后端"
    );
    evidence(
        "dod4_ref_pick_strict",
        &json!({
            "unknown_kind": "invalid_enum",
            "unknown_member": "unknown_field",
            "missing_kind": "missing_field",
            "backend_untouched": fixture.backend.calls().is_empty(),
        }),
    );
}

/// DoD4：选择器错误 → `internal`；未接线 → `not_implemented`。
#[test]
fn ref_pick_maps_picker_error_and_missing_picker() {
    let fixture = build_fixture(
        "ref-pick-error",
        Some(Arc::new(FixedDirectoryPicker::with_file_error(
            "对话框不可用",
        ))),
    );
    let error = invoke(&fixture.webview, "ref_pick", json!({ "kind": "file" }))
        .expect_err("选择器错误必须结构化返回");
    assert_eq!(error["code"], "internal", "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("对话框不可用")),
        "{error}"
    );

    let absent = build_fixture("ref-pick-absent", None);
    let error = invoke(&absent.webview, "ref_pick", json!({ "kind": "file" }))
        .expect_err("未接线选择器必须拒绝");
    assert_eq!(error["code"], "not_implemented", "{error}");
}

/// DoD2（命令层）：`artifact_add` 的路径拒绝在调用后端之前发生（不落库/不透传）。
#[test]
fn artifact_add_rejects_path_before_backend() {
    let fixture = build_fixture("artifact-add-reject", None);
    let missing = temp_dir("artifact-missing").join("nope.txt");
    let error = invoke(
        &fixture.webview,
        "artifact_add",
        json!({ "session_id": "01J0000000000000000000000S", "path": missing.to_string_lossy() }),
    )
    .expect_err("canonicalize 失败必须拒绝");
    assert_eq!(error["code"], "artifact_path_rejected", "{error}");
    assert!(
        fixture.backend.calls().is_empty(),
        "路径拒绝不得触碰业务后端（不落库）"
    );

    // 未知成员同样在解析阶段拒绝。
    let error = invoke(
        &fixture.webview,
        "artifact_add",
        json!({
            "session_id": "01J0000000000000000000000S",
            "path": "C:\\x.txt",
            "unexpected": 1,
        }),
    )
    .expect_err("未知成员必须拒绝");
    assert_eq!(error["code"], "unknown_field", "{error}");
}
