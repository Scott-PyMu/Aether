//! M1-06 命令层断言：启动门阻断、`startup_migrate` 校验与迁移命令、`startup_get`。
//!
//! 通过 `tauri::test` 的 MockRuntime 走真实命令注册路径（`generate_handler` →
//! 参数提取 → 命令体 → 错误序列化），断言：
//! 1. 阻塞态业务命令返回 `startup_blocked` 且不调用后端（主界面不可达的命令层兜底）；
//! 2. `startup_migrate` 畸形参数返回结构化错误且不落库；
//! 3. 合法迁移命令完成「复制 → 校验 → 原子替换 → 指针锁定」，门恢复 Ready。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aether_tauri::ipc::backend::IpcBackend;
use aether_tauri::ipc::dto::SessionListRequest;
use aether_tauri::ipc::error::IpcError;
use aether_tauri::ipc::{handler, IpcState};
use aether_tauri::startup::detect::{
    DetectionContext, NetworkDriveSource, PlatformKind, RegistrySource, ReparseSource,
    MAC_PRECISION_NOTE,
};
use aether_tauri::startup::{pointer, DataDirSource, StartupGate, StartupPhase};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};

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

fn sync_context(sync_root: &Path) -> DetectionContext {
    let mut ctx = allow_context();
    ctx.env
        .insert("OneDrive".to_string(), sync_root.to_path_buf());
    ctx
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
    gate: Arc<StartupGate>,
}

fn fixture(_label: &str, gate: Arc<StartupGate>) -> Fixture {
    let backend = Arc::new(RecordingBackend::default());
    let state = IpcState::with_startup(backend.clone(), Vec::new(), gate.clone());
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
        gate,
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

fn make_source(root: &Path, name: &str) -> PathBuf {
    let source = root.join(name);
    std::fs::create_dir_all(&source).expect("创建源目录");
    std::fs::write(source.join("aether.db"), b"aether-db-bytes").expect("写入主库");
    std::fs::write(source.join("aether.db-wal"), b"wal-bytes").expect("写入 WAL");
    source
}

#[test]
fn malformed_startup_migrate_samples_return_structured_errors() {
    let root = temp_dir("ipc-malformed");
    let ready_dir = root.join("ready");
    std::fs::create_dir_all(&ready_dir).expect("创建就绪目录");
    let gate = Arc::new(StartupGate::bootstrap_at(
        ready_dir,
        DataDirSource::Default,
        allow_context(),
        None,
    ));
    let fixture = fixture("malformed", gate);
    let missing = root.join("missing-target");

    let samples: Vec<(Value, &str, Option<&str>)> = vec![
        (json!({}), "missing_field", Some("target_dir")),
        (
            json!({ "target_dir": "" }),
            "invalid_format",
            Some("target_dir"),
        ),
        (
            json!({ "target_dir": "relative/path" }),
            "path_rejected",
            None,
        ),
        (
            json!({ "target_dir": missing.to_string_lossy() }),
            "path_rejected",
            None,
        ),
        (
            json!({ "target": root.to_string_lossy() }),
            "unknown_field",
            Some("target"),
        ),
    ];

    for (payload, code, field) in samples {
        let result = invoke(&fixture.webview, "startup_migrate", payload.clone());
        match result {
            Ok(value) => panic!("样本 {payload} 期望结构化错误，实际成功：{value}"),
            Err(error) => {
                assert_eq!(error["code"], code, "样本 {payload}：{error}");
                if let Some(field) = field {
                    assert_eq!(error["field"], field, "样本 {payload}：{error}");
                }
            }
        }
        assert!(
            fixture.backend.calls().is_empty(),
            "校验失败不得调用下游：{:?}",
            fixture.backend.calls()
        );
    }
}

#[test]
fn blocked_gate_blocks_feature_commands_and_reports_snapshot() {
    let root = temp_dir("ipc-blocked");
    let source = make_source(&root, "sync-root/Aether");
    let gate = Arc::new(StartupGate::bootstrap_at(
        source,
        DataDirSource::Default,
        sync_context(&root.join("sync-root")),
        Some(root.join("config").join("data-location.json")),
    ));
    let fixture = fixture("blocked", gate);

    let blocked = invoke(&fixture.webview, "session_list", json!({ "limit": 50 }))
        .expect_err("阻塞态业务命令必须失败");
    assert_eq!(blocked["code"], "startup_blocked", "{blocked}");
    assert!(
        fixture.backend.calls().is_empty(),
        "阻塞态不得调用下游：{:?}",
        fixture.backend.calls()
    );

    let snapshot = invoke(&fixture.webview, "startup_get", Value::Null).expect("startup_get");
    assert_eq!(snapshot["phase"], "blocked_sync_dir");
    assert!(snapshot["detection"]["reasons"]
        .as_array()
        .is_some_and(|reasons| !reasons.is_empty()));
    assert_eq!(fixture.gate.snapshot().phase, StartupPhase::BlockedSyncDir);
}

/// DoD2 降级断言（UI 状态码/提示文本）：macOS 阻塞态快照必须携带精度限制文案，
/// 供启动门 `startup-precision-note` 渲染（M1-06 风险条款）。
#[test]
fn macos_blocked_snapshot_exposes_precision_note() {
    let root = temp_dir("ipc-macos-note");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("创建模拟 home");
    let source = home
        .join("Library")
        .join("CloudStorage")
        .join("Dropbox")
        .join("Aether");
    std::fs::create_dir_all(&source).expect("创建模拟同步盘数据目录");
    let mut ctx = allow_context();
    ctx.platform = PlatformKind::MacOs;
    ctx.home = Some(home);
    let gate = Arc::new(StartupGate::bootstrap_at(
        source,
        DataDirSource::Default,
        ctx,
        Some(root.join("config").join("data-location.json")),
    ));
    let fixture = fixture("macos-note", gate);

    let snapshot = invoke(&fixture.webview, "startup_get", Value::Null).expect("startup_get");
    assert_eq!(snapshot["phase"], "blocked_sync_dir", "状态码：{snapshot}");
    assert_eq!(snapshot["detection"]["platform"], "macos");
    assert_eq!(
        snapshot["detection"]["note"].as_str(),
        Some(MAC_PRECISION_NOTE),
        "提示文本必须与降级常量一致"
    );
    let note = snapshot["detection"]["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("检测精度受限"),
        "提示必须含「检测精度受限」：{note}"
    );
    assert!(
        note.contains("请确认目录不在 iCloud/CloudStorage 下"),
        "提示必须含手动确认指引：{note}"
    );
    println!("[m1-06] ipc macos precision-note: {note}");
}

#[test]
fn startup_migrate_command_executes_and_locks_new_dir() {
    let root = temp_dir("ipc-migrate");
    let source = make_source(&root, "sync-root/Aether");
    let pointer_file = root.join("config").join("data-location.json");
    let gate = Arc::new(StartupGate::bootstrap_at(
        source.clone(),
        DataDirSource::Default,
        sync_context(&root.join("sync-root")),
        Some(pointer_file.clone()),
    ));
    let fixture = fixture("migrate", gate);

    let target = root.join("local-target");
    std::fs::create_dir_all(&target).expect("创建迁移目标");
    let migrated = invoke(
        &fixture.webview,
        "startup_migrate",
        json!({ "target_dir": target.to_string_lossy() }),
    )
    .expect("迁移命令成功");

    assert_eq!(migrated["phase"], "ready");
    assert_eq!(migrated["data_dir_source"], "migrated");
    assert!(target.join("aether.db").is_file());
    assert!(source.join("aether.db").is_file(), "源目录必须保留");
    assert_eq!(
        pointer::read_pointer(&pointer_file).expect("读取指针"),
        Some(target.clone())
    );

    let listed = invoke(&fixture.webview, "session_list", json!({ "limit": 50 }))
        .expect("迁移后业务命令恢复");
    assert_eq!(listed["recorded"], "session_list");
    assert_eq!(fixture.backend.calls(), vec!["session_list".to_string()]);
}
