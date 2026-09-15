//! IPC 参数校验框架单测矩阵（M1-08 DoD 3）。
//!
//! 覆盖三类畸形参数（超长 / 未知字段 / 非法枚举）与路径类样本；全部断言：
//! 1. 返回结构化错误码（可选字段名）；
//! 2. 不调用后端（即不落库、不透传下游）。
//!
//! 通过 `tauri::test` 的 MockRuntime 走真实命令注册路径（generate_handler →
//! 参数提取 → 命令体 → 错误序列化），而非直接调用内部函数。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aether_tauri::ipc::backend::IpcBackend;
use aether_tauri::ipc::dto::{
    BackupCreateRequest, ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
};
use aether_tauri::ipc::error::IpcError;
use aether_tauri::ipc::path::{is_within, validate_user_path};
use aether_tauri::ipc::validate::{
    parse_strict, CommandRequest, MAX_MESSAGE_BYTES, MAX_TITLE_CHARS,
};
use aether_tauri::ipc::{handler, IpcState};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};

const ULID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";

/// 记录后端：只为断言「下游是否被调用」。
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

    fn record(&self, command: &str) -> Result<Value, IpcError> {
        match self.calls.lock() {
            Ok(mut guard) => guard.push(command.to_string()),
            Err(poisoned) => poisoned.into_inner().push(command.to_string()),
        }
        Ok(json!({ "recorded": command }))
    }
}

impl IpcBackend for RecordingBackend {
    fn runtimes_list(&self) -> Result<Value, IpcError> {
        self.record("runtimes_list")
    }

    fn session_list(&self, _request: &SessionListRequest) -> Result<Value, IpcError> {
        self.record("session_list")
    }

    fn session_create(&self, _request: &SessionCreateRequest) -> Result<Value, IpcError> {
        self.record("session_create")
    }

    fn session_send(&self, _request: &SessionSendRequest) -> Result<Value, IpcError> {
        self.record("session_send")
    }

    fn session_interrupt(&self, _request: &SessionIdRequest) -> Result<Value, IpcError> {
        self.record("session_interrupt")
    }

    fn session_dispose(&self, _request: &SessionIdRequest) -> Result<Value, IpcError> {
        self.record("session_dispose")
    }

    fn messages_page(&self, _request: &MessagesPageRequest) -> Result<Value, IpcError> {
        self.record("messages_page")
    }

    fn permissions_pending(&self, _request: &PermissionsPendingRequest) -> Result<Value, IpcError> {
        self.record("permissions_pending")
    }

    fn permission_resolve(&self, _request: &PermissionResolveRequest) -> Result<Value, IpcError> {
        self.record("permission_resolve")
    }

    fn settings_get(&self, _request: &SettingsGetRequest) -> Result<Value, IpcError> {
        self.record("settings_get")
    }

    fn settings_set(&self, _request: &SettingsSetRequest) -> Result<Value, IpcError> {
        self.record("settings_set")
    }

    fn backup_create(&self, _request: &BackupCreateRequest) -> Result<Value, IpcError> {
        self.record("backup_create")
    }

    fn export_diagnostics(
        &self,
        _request: &ExportDiagnosticsRequest,
        _canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        self.record("export_diagnostics")
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "aether-m1-08-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

struct Fixture {
    #[allow(dead_code)]
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    backend: Arc<RecordingBackend>,
    root: PathBuf,
    outside: PathBuf,
}

fn fixture(label: &str) -> Fixture {
    let base = temp_dir(label);
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).expect("创建 root");
    std::fs::create_dir_all(&outside).expect("创建 outside");

    let backend = Arc::new(RecordingBackend::default());
    let state = IpcState::new(backend.clone(), vec![root.clone()]);
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
        root,
        outside,
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

fn assert_error(result: Result<Value, Value>, code: &str, field: Option<&str>, label: &str) {
    match result {
        Ok(value) => panic!("样本 {label} 期望结构化错误，实际成功：{value}"),
        Err(error) => {
            assert_eq!(
                error.get("code").and_then(Value::as_str),
                Some(code),
                "样本 {label} 期望错误码 {code}，实际：{error}"
            );
            if let Some(expected_field) = field {
                assert_eq!(
                    error.get("field").and_then(Value::as_str),
                    Some(expected_field),
                    "样本 {label} 期望字段 {expected_field}，实际：{error}"
                );
            }
        }
    }
}

#[test]
fn malformed_samples_return_structured_errors_and_do_not_reach_backend() {
    let fixture = fixture("malformed");
    let overlong_text = "x".repeat(MAX_MESSAGE_BYTES + 1);
    let long_title = "标".repeat(MAX_TITLE_CHARS + 1);
    let outside = fixture.outside.to_string_lossy().to_string();
    let traversal = fixture
        .root
        .join("..")
        .join(fixture.outside.file_name().expect("outside 目录名"))
        .to_string_lossy()
        .to_string();

    let samples: Vec<(&str, Value, &str, Option<&str>)> = vec![
        (
            "session_send",
            json!({ "session_id": ULID, "text": "hi", "unexpected": true }),
            "unknown_field",
            Some("unexpected"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID }),
            "missing_field",
            Some("text"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": overlong_text }),
            "too_large",
            Some("text"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": 42 }),
            "invalid_type",
            None,
        ),
        (
            "session_send",
            json!({ "session_id": "not-a-ulid", "text": "hi" }),
            "invalid_format",
            Some("session_id"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": "bad\u{0}text" }),
            "invalid_format",
            Some("text"),
        ),
        (
            "session_create",
            json!({ "runtime_id": "mock", "title": long_title }),
            "too_large",
            Some("title"),
        ),
        (
            "session_create",
            json!({ "runtime_id": "Claude Code", "title": "ok" }),
            "invalid_format",
            Some("runtime_id"),
        ),
        (
            "session_create",
            json!({ "runtime_id": "mock", "title": "ok", "extra": 1 }),
            "unknown_field",
            Some("extra"),
        ),
        (
            "session_list",
            json!({ "status": "zombie" }),
            "invalid_enum",
            None,
        ),
        (
            "messages_page",
            json!({ "session_id": ULID, "limit": 501 }),
            "out_of_range",
            Some("limit"),
        ),
        (
            "messages_page",
            json!({ "session_id": ULID, "limit": 0 }),
            "out_of_range",
            Some("limit"),
        ),
        (
            "messages_page",
            json!({ "session_id": ULID, "last_seq": -1 }),
            "invalid_value",
            None,
        ),
        (
            "permission_resolve",
            json!({ "request_id": ULID, "decision": "always" }),
            "invalid_enum",
            None,
        ),
        (
            "permission_resolve",
            json!({ "request_id": "short", "decision": "once" }),
            "invalid_format",
            Some("request_id"),
        ),
        (
            "settings_get",
            json!({ "key": "theme" }),
            "invalid_enum",
            Some("key"),
        ),
        (
            "settings_set",
            json!({ "key": "unknown.key", "value": { "a": 1 } }),
            "invalid_enum",
            Some("key"),
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": "relative/path" }),
            "path_rejected",
            None,
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": outside }),
            "path_rejected",
            None,
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": traversal }),
            "path_rejected",
            None,
        ),
        (
            "session_interrupt",
            json!({ "session_id": ["not", "a", "string"] }),
            "invalid_type",
            None,
        ),
        (
            "session_send",
            json!(["not-an-object"]),
            "invalid_json",
            None,
        ),
    ];

    for (command, payload, code, field) in samples {
        let result = invoke(&fixture.webview, command, payload.clone());
        assert_error(result, code, field, &format!("{command} {payload}"));
        assert!(
            fixture.backend.calls().is_empty(),
            "校验失败后不得调用下游（不落库/不透传）：{:?}",
            fixture.backend.calls()
        );
    }
}

#[test]
fn valid_requests_reach_backend_exactly_once() {
    let fixture = fixture("valid");
    let inside = fixture.root.to_string_lossy().to_string();

    let samples: Vec<(&str, Value, &str)> = vec![
        ("runtimes_list", Value::Null, "runtimes_list"),
        ("session_list", json!({ "limit": 50 }), "session_list"),
        (
            "session_create",
            json!({ "runtime_id": "mock", "title": "会话", "model": "deepseek/deepseek-chat" }),
            "session_create",
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": "你好", "client_msg_id": "msg-1" }),
            "session_send",
        ),
        (
            "session_interrupt",
            json!({ "session_id": ULID }),
            "session_interrupt",
        ),
        (
            "session_dispose",
            json!({ "session_id": ULID }),
            "session_dispose",
        ),
        (
            "messages_page",
            json!({ "session_id": ULID, "last_seq": 0, "limit": 500 }),
            "messages_page",
        ),
        ("permissions_pending", json!({}), "permissions_pending"),
        (
            "permission_resolve",
            json!({ "request_id": ULID, "decision": "session" }),
            "permission_resolve",
        ),
        (
            "backup_create",
            json!({ "label": "手动备份" }),
            "backup_create",
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": inside }),
            "export_diagnostics",
        ),
    ];

    for (command, payload, expected) in samples {
        let result = invoke(&fixture.webview, command, payload.clone());
        let value = result.unwrap_or_else(|error| {
            panic!("样本 {command} {payload} 应通过校验，实际错误：{error}")
        });
        assert_eq!(
            value.get("recorded").and_then(Value::as_str),
            Some(expected),
            "命令 {command} 必须到达后端"
        );
    }

    // settings_* 的键白名单当前为空（默认拒绝），合法形态也应返回 invalid_enum。
    for (command, payload) in [
        ("settings_get", json!({ "key": "theme" })),
        ("settings_set", json!({ "key": "theme", "value": true })),
    ] {
        assert_error(
            invoke(&fixture.webview, command, payload),
            "invalid_enum",
            Some("key"),
            command,
        );
    }

    let expected_calls: Vec<String> = [
        "runtimes_list",
        "session_list",
        "session_create",
        "session_send",
        "session_interrupt",
        "session_dispose",
        "messages_page",
        "permissions_pending",
        "permission_resolve",
        "backup_create",
        "export_diagnostics",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        fixture.backend.calls(),
        expected_calls,
        "合法请求必须恰好一次到达后端"
    );
}

#[cfg(windows)]
#[test]
fn windows_special_path_samples_are_rejected_without_downstream_call() {
    let fixture = fixture("windows-paths");
    let samples = [
        r"\\server\share\dir",
        r"\\?\C:\data",
        r"\\.\C:\data",
        r"C:\data\PROGRA~1\dir",
        r"C:\data\file.txt:stream",
        r"C:\data\trailing.",
        r"C:\data\trailing ",
        r"C:\data\CON",
        r"C:\data\nul.txt",
        r"C:\data\LPT1.log",
    ];
    for sample in samples {
        let result = invoke(
            &fixture.webview,
            "export_diagnostics",
            json!({ "target_dir": sample }),
        );
        match result {
            Ok(value) => panic!("样本 {sample} 应被拒绝，实际成功：{value}"),
            Err(error) => {
                assert_eq!(error["code"], "path_rejected", "样本 {sample}：{error}");
            }
        }
        assert!(
            fixture.backend.calls().is_empty(),
            "路径校验失败后不得调用下游：{:?}",
            fixture.backend.calls()
        );
    }
}

#[test]
fn path_validator_accepts_inside_and_rejects_escape() {
    let root = temp_dir("path-root");
    let inside = root.join("nested");
    std::fs::create_dir_all(&inside).expect("创建子目录");

    assert!(validate_user_path(&inside.to_string_lossy(), std::slice::from_ref(&root)).is_ok());
    assert!(validate_user_path(&root.to_string_lossy(), std::slice::from_ref(&root)).is_ok());

    let outside = temp_dir("path-outside");
    let escape = root
        .join("..")
        .join(outside.file_name().expect("outside 目录名"));
    assert_eq!(
        validate_user_path(&escape.to_string_lossy(), std::slice::from_ref(&root))
            .expect_err("逃逸必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
    assert_eq!(
        validate_user_path(&outside.to_string_lossy(), &[root])
            .expect_err("根目录外必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
}

#[test]
fn path_prefix_check_is_component_wise() {
    let root = PathBuf::from(if cfg!(windows) { r"C:\data" } else { "/data" });
    let sibling = PathBuf::from(if cfg!(windows) {
        r"C:\database"
    } else {
        "/database"
    });
    let inside = PathBuf::from(if cfg!(windows) {
        r"C:\data\file.txt"
    } else {
        "/data/file.txt"
    });
    assert!(is_within(&inside, &root));
    assert!(!is_within(&sibling, &root));
}

#[test]
fn parse_strict_classifies_serde_errors() {
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Sample {
        name: String,
        #[serde(default)]
        optional: Option<u32>,
    }

    impl CommandRequest for Sample {}

    let unknown = parse_strict::<Sample>(json!({ "name": "a", "extra": 1 })).expect_err("未知字段");
    assert_eq!(unknown.code.as_str(), "unknown_field");
    assert_eq!(unknown.field.as_deref(), Some("extra"));

    let missing = parse_strict::<Sample>(json!({})).expect_err("缺字段");
    assert_eq!(missing.code.as_str(), "missing_field");
    assert_eq!(missing.field.as_deref(), Some("name"));

    let wrong_type = parse_strict::<Sample>(json!({ "name": 42 })).expect_err("类型错误");
    assert_eq!(wrong_type.code.as_str(), "invalid_type");

    let not_object = parse_strict::<Sample>(json!([1, 2])).expect_err("非对象");
    assert_eq!(not_object.code.as_str(), "invalid_json");

    assert!(parse_strict::<Sample>(json!({ "name": "ok", "optional": 3 })).is_ok());
}
