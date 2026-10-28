//! M9-04 代表性 golden（P2 计划 M9-04 DoD-3）。
//!
//! 口径：请求按**内层 DTO**（无 `{payload}` 包装）规范化；后端返回脚本化响应，
//! 命令层必须原样透传（结构化比较）。未改契约命令与 `docs/M9-01-baseline/` 的
//! 0 差异由 `scripts/test/m9-04/verify-m9-04.mjs` 的哈希对照断言。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_app::ipc::backend::IpcBackend;
use aether_app::ipc::dto::{
    AuditQueryRequest, BackupCreateRequest, BackupRestoreRequest, MessagesPageRequest,
    PermissionResolveRequest, PermissionsPendingRequest, ProvidersListRequest,
    SessionCreateRequest, SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
};
use aether_app::ipc::{IpcError, IpcState};
use aether_app::shell_host::ShellHost;
use serde_json::{json, Value};

#[derive(Default)]
struct ScriptedBackend {
    responses: HashMap<String, Value>,
}

impl ScriptedBackend {
    fn respond(&self, command: &str) -> Result<Value, IpcError> {
        self.responses
            .get(command)
            .cloned()
            .ok_or_else(|| IpcError::internal(format!("缺少脚本化响应：{command}")))
    }
}

impl IpcBackend for ScriptedBackend {
    fn health(&self) -> Result<Value, IpcError> {
        self.respond("health")
    }

    fn session_create(&self, _request: &SessionCreateRequest) -> Result<Value, IpcError> {
        self.respond("session_create")
    }

    fn session_send(&self, _request: &SessionSendRequest) -> Result<Value, IpcError> {
        self.respond("session_send")
    }

    fn messages_page(&self, _request: &MessagesPageRequest) -> Result<Value, IpcError> {
        self.respond("messages_page")
    }

    fn permissions_pending(&self, _request: &PermissionsPendingRequest) -> Result<Value, IpcError> {
        self.respond("permissions_pending")
    }

    fn permission_resolve(&self, _request: &PermissionResolveRequest) -> Result<Value, IpcError> {
        self.respond("permission_resolve")
    }

    fn settings_get(&self, _request: &SettingsGetRequest) -> Result<Value, IpcError> {
        self.respond("settings_get")
    }

    fn settings_set(&self, _request: &SettingsSetRequest) -> Result<Value, IpcError> {
        self.respond("settings_set")
    }

    fn backup_create(
        &self,
        _request: &BackupCreateRequest,
        _canonical_target_dir: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.respond("backup_create")
    }

    fn backup_list(&self) -> Result<Value, IpcError> {
        self.respond("backup_list")
    }

    fn backup_restore(
        &self,
        _request: &BackupRestoreRequest,
        _canonical_external_path: Option<&Path>,
    ) -> Result<Value, IpcError> {
        self.respond("backup_restore")
    }

    fn audit_query(&self, _request: &AuditQueryRequest) -> Result<Value, IpcError> {
        self.respond("audit_query")
    }

    fn providers_list(&self, _request: &ProvidersListRequest) -> Result<Value, IpcError> {
        self.respond("providers_list")
    }
}

#[derive(Default)]
struct FakeShell {
    restarts: std::sync::atomic::AtomicUsize,
}

impl ShellHost for FakeShell {
    fn exit(&self, _code: i32) {}

    fn request_restart(&self) {
        self.restarts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-04")
        .join("fixtures")
}

fn replace(value: &Value, ulid: &str, client: &str) -> Value {
    match value {
        Value::String(text) if text == "$ULID" => Value::from(ulid),
        Value::String(text) if text == "$CLIENT_MSG_ID" => Value::from(client),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace(item, ulid, client))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), replace(item, ulid, client)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn m9_04_representative_goldens_match_structured_responses() {
    let matrix: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("golden.json")).expect("读取 golden"),
    )
    .expect("解析 golden");
    assert_eq!(matrix["schema"], json!("aether.m9-04.golden.v1"));
    let cases = matrix["cases"].as_array().expect("cases");
    assert!(cases.len() >= 13, "代表性 golden 至少 13 条");
    let ulid = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";
    let client = "01J8ZQ5R0N7W9Y8X6V4T2S0K1N";
    let mut responses = HashMap::new();
    for case in cases {
        responses.insert(
            case["command"].as_str().expect("command").to_owned(),
            replace(&case["expected"], ulid, client),
        );
    }
    let backend = Arc::new(ScriptedBackend { responses });
    let state = IpcState::new(backend, Vec::new());
    let shell = Arc::new(FakeShell::default());
    let _ = state.install_shell(shell.clone());

    let mut groups = std::collections::BTreeSet::new();
    for case in cases {
        let command = case["command"].as_str().expect("command");
        groups.insert(case["group"].as_str().expect("group").to_owned());
        let params = replace(&case["params"], ulid, client);
        let actual = state
            .dispatch(command, params)
            .unwrap_or_else(|error| panic!("{command} golden 失败：{error}"));
        let expected = replace(&case["expected"], ulid, client);
        assert_eq!(actual, expected, "{command} 响应必须原样透传");
    }
    for group in [
        "session",
        "messages",
        "health",
        "backup",
        "permissions",
        "settings",
        "audit",
        "providers",
    ] {
        assert!(groups.contains(group), "缺少分组 {group} 的代表性 golden");
    }
    assert_eq!(
        shell.restarts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "backup_restore.restart_required=true 必须触发壳层重启"
    );
}

#[test]
fn m9_04_payload_wrapper_is_rejected_for_golden_requests() {
    let state = IpcState::new(Arc::new(ScriptedBackend::default()), Vec::new());
    for (command, params) in [
        ("session_create", json!({ "payload": { "title": "会话" } })),
        (
            "settings_get",
            json!({ "payload": { "key": "backup.reminder" } }),
        ),
        (
            "messages_page",
            json!({ "payload": { "session_id": "01J8ZQ5R0N7W9Y8X6V4T2S0K1M" } }),
        ),
    ] {
        match state.dispatch(command, params) {
            Ok(value) => panic!("{command} 不得接受 {{payload}} 包装：{value}"),
            Err(error) => {
                assert_eq!(error.code.as_str(), "unknown_field", "{command}：{error}");
                assert_eq!(
                    error.field.as_deref(),
                    Some("payload"),
                    "{command}：{error}"
                );
            }
        }
    }
}
