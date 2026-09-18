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
    AppRestartRequest, BackupCreateRequest, BackupRestoreRequest, ExportDiagnosticsRequest,
    MessagesPageRequest, PermissionResolveRequest, PermissionsPendingRequest, RunRetryRequest,
    RuntimeEnableRequest, RuntimeRetryRequest, SessionCreateRequest, SessionIdRequest,
    SessionListRequest, SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
    WorkspaceSetRequest,
};
use aether_tauri::ipc::error::IpcError;
use aether_tauri::ipc::path::{
    is_within, validate_external_file, validate_user_path, validate_workspace_root,
};
use aether_tauri::ipc::validate::{
    parse_strict, CommandRequest, MAX_MESSAGE_BYTES, MAX_TITLE_CHARS,
};
use aether_tauri::ipc::{handler, IpcState};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};

const ULID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";
const CLIENT_MSG_ID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1N";

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

    fn backup_list(&self) -> Result<Value, IpcError> {
        self.record("backup_list")
    }

    /// ADR-007 决策 1：health（无参数；记录下游调用以便断言）。
    fn health(&self) -> Result<Value, IpcError> {
        self.record("health")
    }

    fn backup_restore(
        &self,
        _request: &BackupRestoreRequest,
        _canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.record("backup_restore")
    }

    fn app_restart(&self, _request: &AppRestartRequest) -> Result<Value, IpcError> {
        self.record("app_restart")
    }

    fn run_retry(&self, _request: &RunRetryRequest) -> Result<Value, IpcError> {
        self.record("run_retry")
    }

    fn runtime_retry(&self, _request: &RuntimeRetryRequest) -> Result<Value, IpcError> {
        self.record("runtime_retry")
    }

    fn runtime_enable(&self, _request: &RuntimeEnableRequest) -> Result<Value, IpcError> {
        self.record("runtime_enable")
    }

    fn workspace_set(
        &self,
        _request: &WorkspaceSetRequest,
        _canonical_root_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.record("workspace_set")
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

/// 长路径形式（测试夹具用）。
///
/// CI 的 `%TEMP%` 形如 `C:\Users\RUNNER~1\...`，含 8.3 短名——短名是 D9/T7 的
/// **拒绝样本**，不能直接当作合法输入；夹具先 canonicalize 并去掉 `\\?\` 前缀。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
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
    let external_db = fixture.outside.join("candidate.db");
    std::fs::write(&external_db, b"not a database").expect("写入外部候选 .db");
    let external_txt = fixture.outside.join("candidate.txt");
    std::fs::write(&external_txt, b"x").expect("写入外部候选 .txt");
    let missing_db = fixture.outside.join("missing.db");
    let sync_root = fixture.outside.join("OneDrive").join("workspace");
    std::fs::create_dir_all(&sync_root).expect("创建同步盘样本目录");
    let inside = fixture.root.to_string_lossy().to_string();
    let traversal = fixture
        .root
        .join("..")
        .join(fixture.outside.file_name().expect("outside 目录名"))
        .to_string_lossy()
        .to_string();

    let samples: Vec<(&str, Value, &str, Option<&str>)> = vec![
        (
            "session_send",
            json!({ "session_id": ULID, "text": "hi", "client_msg_id": CLIENT_MSG_ID, "unexpected": true }),
            "unknown_field",
            Some("unexpected"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "client_msg_id": CLIENT_MSG_ID }),
            "missing_field",
            Some("text"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": "hi" }),
            "missing_field",
            Some("client_msg_id"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": "hi", "client_msg_id": "msg-1" }),
            "invalid_format",
            Some("client_msg_id"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": overlong_text, "client_msg_id": CLIENT_MSG_ID }),
            "too_large",
            Some("text"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": 42, "client_msg_id": CLIENT_MSG_ID }),
            "invalid_type",
            None,
        ),
        (
            "session_send",
            json!({ "session_id": "not-a-ulid", "text": "hi", "client_msg_id": CLIENT_MSG_ID }),
            "invalid_format",
            Some("session_id"),
        ),
        (
            "session_send",
            json!({ "session_id": ULID, "text": "bad\u{0}text", "client_msg_id": CLIENT_MSG_ID }),
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
        // ===== ADR-004 七命令校验矩阵 =====
        (
            "backup_list",
            json!({ "unexpected": 1 }),
            "unknown_field",
            Some("unexpected"),
        ),
        ("backup_restore", json!({}), "missing_field", Some("source")),
        (
            "backup_restore",
            json!({ "source": { "internal": { "id": "short" } } }),
            "invalid_format",
            Some("source.internal.id"),
        ),
        (
            "backup_restore",
            json!({ "source": { "external": { "path": "relative.db" } } }),
            "path_rejected",
            None,
        ),
        (
            "backup_restore",
            json!({ "source": { "external": { "path": external_txt.to_string_lossy() } } }),
            "path_rejected",
            None,
        ),
        (
            "backup_restore",
            json!({ "source": { "external": { "path": missing_db.to_string_lossy() } } }),
            "path_rejected",
            None,
        ),
        ("app_restart", json!({}), "missing_field", Some("confirm")),
        (
            "app_restart",
            json!({ "confirm": false }),
            "invalid_value",
            Some("confirm"),
        ),
        (
            "app_restart",
            json!({ "confirm": "yes" }),
            "invalid_type",
            None,
        ),
        (
            "run_retry",
            json!({ "run_id": "short" }),
            "invalid_format",
            Some("run_id"),
        ),
        (
            "run_retry",
            json!({ "run": ULID }),
            "unknown_field",
            Some("run"),
        ),
        (
            "runtime_retry",
            json!({ "runtime_id": "Claude Code" }),
            "invalid_format",
            Some("runtime_id"),
        ),
        (
            "runtime_enable",
            json!({}),
            "missing_field",
            Some("runtime_id"),
        ),
        ("workspace_set", json!({}), "invalid_value", None),
        (
            "workspace_set",
            json!({ "workspace_id": ULID, "root_path": inside }),
            "invalid_value",
            None,
        ),
        (
            "workspace_set",
            json!({ "root_path": "relative" }),
            "path_rejected",
            None,
        ),
        (
            "workspace_set",
            json!({ "root_path": sync_root.to_string_lossy() }),
            "path_rejected",
            None,
        ),
        (
            "session_send",
            json!(["not-an-object"]),
            "invalid_json",
            None,
        ),
        // ===== ADR-007 决策 1：health 无参数严格解析（任何成员拒绝） =====
        (
            "health",
            json!({ "unexpected": 1 }),
            "unknown_field",
            Some("unexpected"),
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
    let inside = long_path(&fixture.root);
    let external_db = fixture.outside.join("restore-candidate.db");
    std::fs::write(&external_db, b"candidate").expect("写入外部候选 .db");
    let external_db = long_path(&external_db);

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
            json!({ "session_id": ULID, "text": "你好", "client_msg_id": CLIENT_MSG_ID }),
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
        // ADR-004 七命令：合法形态必须到达后端（含无参数 backup_list 的两种调用方式）。
        ("backup_list", Value::Null, "backup_list"),
        ("backup_list", json!({}), "backup_list"),
        (
            "backup_restore",
            json!({ "source": { "internal": { "id": ULID } } }),
            "backup_restore",
        ),
        (
            "backup_restore",
            json!({ "source": { "external": { "path": external_db } } }),
            "backup_restore",
        ),
        ("app_restart", json!({ "confirm": true }), "app_restart"),
        ("run_retry", json!({ "run_id": ULID }), "run_retry"),
        (
            "runtime_retry",
            json!({ "runtime_id": "mock" }),
            "runtime_retry",
        ),
        (
            "runtime_enable",
            json!({ "runtime_id": "mock" }),
            "runtime_enable",
        ),
        (
            "workspace_set",
            json!({ "workspace_id": ULID }),
            "workspace_set",
        ),
        (
            "workspace_set",
            json!({ "root_path": inside }),
            "workspace_set",
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": inside }),
            "export_diagnostics",
        ),
        // ADR-007 决策 1：health 无参数（缺省载荷与空对象两种合法调用方式）。
        ("health", Value::Null, "health"),
        ("health", json!({}), "health"),
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
        "backup_list",
        "backup_list",
        "backup_restore",
        "backup_restore",
        "app_restart",
        "run_retry",
        "runtime_retry",
        "runtime_enable",
        "workspace_set",
        "workspace_set",
        "export_diagnostics",
        "health",
        "health",
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

    assert!(validate_user_path(&long_path(&inside), std::slice::from_ref(&root)).is_ok());
    assert!(validate_user_path(&long_path(&root), std::slice::from_ref(&root)).is_ok());

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
fn external_and_workspace_path_validators_follow_d13_d7() {
    let base = temp_dir("path-external");
    let base_long = long_path(&base);

    // backup_restore 外部候选：必须存在、是文件、后缀 .db（大小写不敏感）。
    let db = base.join("backup.DB");
    std::fs::write(&db, b"x").expect("写入候选");
    assert!(validate_external_file(&long_path(&db), "db").is_ok());
    let txt = base.join("backup.txt");
    std::fs::write(&txt, b"x").expect("写入候选");
    assert_eq!(
        validate_external_file(&long_path(&txt), "db")
            .expect_err("非 .db 必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
    let missing = PathBuf::from(&base_long).join("missing.db");
    assert_eq!(
        validate_external_file(&missing.to_string_lossy(), "db")
            .expect_err("不存在必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
    assert_eq!(
        validate_external_file(&base_long, "db")
            .expect_err("目录必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );

    // workspace_set root_path：必须存在且为目录；同步盘路径段拒绝（A4 预检）。
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).expect("创建 workspace");
    assert!(validate_workspace_root(&long_path(&workspace)).is_ok());
    let missing_ws = PathBuf::from(&base_long).join("missing");
    assert_eq!(
        validate_workspace_root(&missing_ws.to_string_lossy())
            .expect_err("不存在必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
    assert_eq!(
        validate_workspace_root(&long_path(&db))
            .expect_err("文件必须拒绝")
            .code
            .as_str(),
        "path_rejected"
    );
    let sync = base.join("OneDrive").join("ws");
    std::fs::create_dir_all(&sync).expect("创建同步盘样本");
    let rejection = validate_workspace_root(&long_path(&sync)).expect_err("同步盘必须拒绝");
    assert_eq!(rejection.code.as_str(), "path_rejected");
    assert!(
        rejection.message.contains("同步盘") || rejection.message.contains("OneDrive"),
        "拒绝原因必须可读：{rejection}"
    );
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
