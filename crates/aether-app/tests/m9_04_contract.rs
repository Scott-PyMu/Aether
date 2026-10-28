//! M9-04 契约矩阵（P2 计划 M9-04 DoD-2/DoD-5；ADR-029 D-029-4）。
//!
//! 覆盖口径：
//! - **63 条命令**逐条合法/非法参数矩阵（合法样本来自 `command-matrix.json`；
//!   非法样本来自 `docs/M9-01-baseline/commands/*.json` 的 `rejects`/`semantic_cases`
//!   + `extra_invalid`，覆盖未知字段/类型/枚举/长度/ULID/路径/越界）；
//! - `IpcErrorCode` **32 枚举** + `BusinessCode` **9 业务码**逐项可达（自然触发或
//!   注入触发；业务码经 `map_ipc_error` 的线上形状由 daemon 测试断言）；
//! - 壳本地 4 命令（`app_exit`/`app_restart`/`ref_pick`/`startup_pick_target`）的
//!   退出/重启/选择/取消路径（真实 Electron E2E 属 M9-07）。
//!
//! 本文件只走应用层 `dispatch`（transport-agnostic）；JSON-RPC 路由/错误封装由
//! `crates/aether-daemon/tests/m9_04_routes.rs` 覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aether_app::ipc::backend::IpcBackend;
use aether_app::ipc::cursor;
use aether_app::ipc::error::{IpcError, IpcErrorCode};
use aether_app::ipc::{BusinessCode, IpcState, COMMANDS, SHELL_COMMANDS};
use aether_app::picker::FixedDirectoryPicker;
use aether_app::shell_host::ShellHost;
use serde_json::{json, Value};

const ULID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";
const CLIENT_MSG_ID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1N";

const VALIDATION_CODES: &[IpcErrorCode] = &[
    IpcErrorCode::InvalidJson,
    IpcErrorCode::UnknownField,
    IpcErrorCode::MissingField,
    IpcErrorCode::InvalidType,
    IpcErrorCode::InvalidValue,
    IpcErrorCode::InvalidEnum,
    IpcErrorCode::TooLarge,
    IpcErrorCode::OutOfRange,
    IpcErrorCode::InvalidFormat,
    IpcErrorCode::PathRejected,
    IpcErrorCode::ArtifactPathRejected,
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-04")
        .join("fixtures")
}

fn baseline_commands_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("M9-01-baseline")
        .join("commands")
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("读取夹具失败 {}：{error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("解析夹具失败 {}：{error}", path.display()))
}

/// 默认后端：全部方法走 `IpcBackend` 默认实现（`not_implemented`）。
#[derive(Default)]
struct EmptyBackend;

impl IpcBackend for EmptyBackend {}

/// 注入后端：`health` 返回指定 `IpcErrorCode`（可附业务码）。
struct FailBackend {
    code: IpcErrorCode,
    business: Option<BusinessCode>,
}

impl IpcBackend for FailBackend {
    fn health(&self) -> Result<Value, IpcError> {
        let error = IpcError::new(self.code, format!("注入错误码 {}", self.code.as_str()));
        Err(match self.business {
            Some(business) => error.with_business_code(business),
            None => error,
        })
    }
}

#[derive(Default)]
struct FakeShell {
    exits: Mutex<Vec<i32>>,
    restarts: AtomicUsize,
}

impl ShellHost for FakeShell {
    fn exit(&self, code: i32) {
        match self.exits.lock() {
            Ok(mut exits) => exits.push(code),
            Err(poisoned) => poisoned.into_inner().push(code),
        }
    }

    fn request_restart(&self) {
        self.restarts.fetch_add(1, Ordering::SeqCst);
    }
}

struct Fixture {
    state: IpcState,
    shell: Arc<FakeShell>,
    root: PathBuf,
    file: PathBuf,
    missing: PathBuf,
}

fn fixture(label: &str) -> Fixture {
    let base = std::env::temp_dir().join(format!(
        "aether-m9-04-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let root = base.join("root");
    std::fs::create_dir_all(&root).expect("创建夹具 root");
    let file = root.join("reference.md");
    std::fs::write(&file, b"# reference").expect("写入引用文件");
    let missing = base.join("missing.txt");
    let shell = Arc::new(FakeShell::default());
    let state = IpcState::new(Arc::new(EmptyBackend), vec![root.clone()]);
    let _ = state.install_shell(shell.clone());
    let _ = state.set_picker(Arc::new(
        FixedDirectoryPicker::new()
            .then_file_path(file.clone())
            .then_file_cancel()
            .then_path(root.clone())
            .then_cancel(),
    ));
    Fixture {
        state,
        shell,
        root,
        file,
        missing,
    }
}

fn replace_placeholders(value: &Value, fixture: &Fixture) -> Value {
    match value {
        Value::String(text) => match text.as_str() {
            "$ULID" => Value::from(ULID),
            "$CLIENT_MSG_ID" => Value::from(CLIENT_MSG_ID),
            "$ROOT" => Value::from(fixture.root.to_string_lossy().to_string()),
            "$FILE" => Value::from(fixture.file.to_string_lossy().to_string()),
            "$MISSING" => Value::from(fixture.missing.to_string_lossy().to_string()),
            "$LONG_TITLE" => Value::from("x".repeat(257)),
            "$LONG_PRESET_NAME" => Value::from("x".repeat(65)),
            "$TOO_LARGE_TEXT" => Value::from("x".repeat(1024 * 1024 + 1)),
            other => Value::from(other),
        },
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace_placeholders(item, fixture))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), replace_placeholders(item, fixture)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn assert_error(result: Result<Value, IpcError>, code: &str, field: Option<&str>, label: &str) {
    match result {
        Ok(value) => panic!("{label} 应被拒绝，实际成功：{value}"),
        Err(error) => {
            assert_eq!(error.code.as_str(), code, "{label} 错误码不符：{error}");
            if let Some(field) = field {
                assert_eq!(
                    error.field.as_deref(),
                    Some(field),
                    "{label} 字段不符：{error}"
                );
            }
        }
    }
}

fn assert_not_validation_failure(result: Result<Value, IpcError>, label: &str) {
    match result {
        Ok(_) => {}
        Err(error) => {
            assert!(
                !VALIDATION_CODES.contains(&error.code),
                "{label} 合法参数被校验拒绝：{error}"
            );
        }
    }
}

#[test]
fn m9_04_baseline_reject_matrix_covers_all_63_commands() {
    let fixture = fixture("baseline");
    let mut files: Vec<PathBuf> = std::fs::read_dir(baseline_commands_dir())
        .expect("读取基线命令目录")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 63, "基线条目必须 63 条");
    let mut seen = Vec::new();
    let mut rejects = 0usize;
    let mut semantic = 0usize;
    for file in files {
        let entry = read_json(&file);
        let command = entry["command"].as_str().expect("命令名").to_owned();
        seen.push(command.clone());
        assert!(
            COMMANDS.contains(&command.as_str()),
            "基线命令 {command} 不在 COMMANDS"
        );
        for reject in entry["rejects"].as_array().into_iter().flatten() {
            let params = replace_placeholders(&reject["params"], &fixture);
            let code = reject["expect"]["code"].as_str().expect("期望错误码");
            let field = reject["expect"]["field"].as_str();
            assert_error(
                fixture.state.dispatch(&command, params),
                code,
                field,
                &format!("{command}/{}", reject["id"]),
            );
            rejects += 1;
        }
        for case in entry["semantic_cases"].as_array().into_iter().flatten() {
            let params = replace_placeholders(&case["params"], &fixture);
            let code = case["expect"]["code"].as_str().expect("期望错误码");
            let field = case["expect"]["field"].as_str();
            assert_error(
                fixture.state.dispatch(&command, params),
                code,
                field,
                &format!("{command}/{}", case["id"]),
            );
            semantic += 1;
        }
    }
    assert_eq!(seen.len(), 63);
    for command in COMMANDS {
        assert!(
            seen.iter().any(|name| name == command),
            "基线缺少 {command}"
        );
    }
    assert!(rejects >= 63, "rejects 样本数 {rejects} < 63");
    assert!(semantic >= 5, "semantic 样本数 {semantic}");
}

#[test]
fn m9_04_valid_request_matrix_covers_all_63_commands() {
    let fixture = fixture("valid");
    let matrix = read_json(&fixtures_dir().join("command-matrix.json"));
    assert_eq!(matrix["schema"], json!("aether.m9-04.command-matrix.v1"));
    let entries = matrix["commands"].as_array().expect("commands");
    assert_eq!(entries.len(), 63, "合法矩阵必须 63 条");
    let mut seen = Vec::new();
    for entry in entries {
        let command = entry["command"].as_str().expect("command");
        seen.push(command.to_owned());
        let params = replace_placeholders(&entry["valid"], &fixture);
        assert_not_validation_failure(
            fixture.state.dispatch(command, params),
            &format!("{command}/valid"),
        );
        for case in entry["extra_invalid"].as_array().into_iter().flatten() {
            let params = replace_placeholders(&case["params"], &fixture);
            let code = case["code"].as_str().expect("code");
            let field = case["field"].as_str();
            assert_error(
                fixture.state.dispatch(command, params),
                code,
                field,
                &format!("{command}/{}", case["id"]),
            );
        }
    }
    assert_eq!(seen.len(), 63);
    for command in COMMANDS {
        assert!(
            seen.iter().any(|name| name == command),
            "矩阵缺少 {command}"
        );
    }
    for shell in SHELL_COMMANDS {
        assert!(
            seen.iter().any(|name| name == shell),
            "矩阵缺少壳命令 {shell}"
        );
    }
}

#[test]
fn m9_04_error_code_matrix_is_32_plus_9_and_all_reachable() {
    let fixture = fixture("errors");
    let matrix = read_json(&fixtures_dir().join("error-code-matrix.json"));
    assert_eq!(matrix["schema"], json!("aether.m9-04.error-code-matrix.v1"));
    let ipc = matrix["ipc"].as_array().expect("ipc");
    let business = matrix["business"].as_array().expect("business");
    assert_eq!(ipc.len(), 32, "32 枚举逐项");
    assert_eq!(business.len(), 9, "9 业务码逐项");
    assert_eq!(IpcErrorCode::ALL.len(), 32);
    assert_eq!(BusinessCode::ALL.len(), 9);

    let mut seen_ipc = Vec::new();
    for entry in ipc {
        let code_text = entry["code"].as_str().expect("code");
        let code = IpcErrorCode::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == code_text)
            .unwrap_or_else(|| panic!("未知 ipc 码 {code_text}"));
        seen_ipc.push(code_text.to_owned());
        match entry["mode"].as_str().expect("mode") {
            "natural" => {
                let command = entry["command"].as_str().expect("command");
                let params = replace_placeholders(&entry["params"], &fixture);
                assert_error(
                    fixture.state.dispatch(command, params),
                    code_text,
                    None,
                    &format!("natural/{code_text}"),
                );
            }
            "injected" => {
                let state = IpcState::new(
                    Arc::new(FailBackend {
                        code,
                        business: None,
                    }),
                    Vec::new(),
                );
                assert_error(
                    state.dispatch("health", Value::Null),
                    code_text,
                    None,
                    &format!("injected/{code_text}"),
                );
            }
            mode => panic!("未知 mode {mode}"),
        }
    }
    assert_eq!(seen_ipc.len(), 32);

    for entry in business {
        let code_text = entry["code"].as_str().expect("code");
        let code =
            BusinessCode::from_wire(code_text).unwrap_or_else(|| panic!("未知业务码 {code_text}"));
        match entry["mode"].as_str().expect("mode") {
            "injected" => {
                let state = IpcState::new(
                    Arc::new(FailBackend {
                        code: IpcErrorCode::InvalidValue,
                        business: Some(code),
                    }),
                    Vec::new(),
                );
                let error = state
                    .dispatch("health", Value::Null)
                    .expect_err("注入业务码必须失败");
                assert_eq!(error.code, IpcErrorCode::InvalidValue);
                assert_eq!(error.business_code(), Some(code));
                let serialized = serde_json::to_value(&error).expect("错误序列化");
                assert!(
                    serialized.get("business_code").is_none(),
                    "IpcError 线上结构不得直接携带 business_code（由 JSON-RPC data 承载）"
                );
            }
            "bootstrap" => {
                // M9-03 进程级 fixture 覆盖真实首帧；此处锁定常量/编码路径。
                let error =
                    IpcError::new(IpcErrorCode::InvalidValue, "bootstrap").with_business_code(code);
                assert_eq!(error.business_code(), Some(code));
            }
            mode => panic!("未知 mode {mode}"),
        }
    }
}

#[test]
fn m9_04_budget_and_cursor_contract_is_exposed_via_commands() {
    let fixture = fixture("budget");
    let max = cursor::MAX_BYTES_MAX;
    for params in [
        json!({ "session_id": ULID, "max_bytes": 65535 }),
        json!({ "session_id": ULID, "max_bytes": max + 1 }),
        json!({ "session_id": ULID, "last_seq": 1, "next_cursor": "opaque" }),
        json!({ "session_id": ULID, "next_cursor": "not*base64url" }),
    ] {
        let result = fixture.state.dispatch("messages_page", params.clone());
        assert!(result.is_err(), "非法预算/游标必须拒绝：{params}");
    }
    for params in [
        json!({ "max_bytes": 65535 }),
        json!({ "max_bytes": max + 1 }),
        json!({ "offset": 1, "next_cursor": "opaque" }),
        json!({ "next_cursor": "not*base64url" }),
    ] {
        let result = fixture.state.dispatch("audit_query", params.clone());
        assert!(result.is_err(), "非法预算/游标必须拒绝：{params}");
    }
    // 合法预算通过校验（后端默认 not_implemented，不是校验失败）。
    assert_not_validation_failure(
        fixture.state.dispatch(
            "messages_page",
            json!({ "session_id": ULID, "max_bytes": 65536 }),
        ),
        "messages_page/max_bytes=64KiB",
    );
    assert_not_validation_failure(
        fixture
            .state
            .dispatch("audit_query", json!({ "max_bytes": 1536 * 1024 })),
        "audit_query/max_bytes=1.5MiB",
    );
}

#[test]
fn m9_04_shell_local_exit_restart_pick_and_cancel_paths() {
    let fixture = fixture("shell");
    // 退出：confirm=true 才进入壳层；缺 confirm → missing_field。
    let value = fixture
        .state
        .dispatch("app_exit", json!({ "confirm": true }))
        .expect("app_exit 应成功");
    assert_eq!(value["exiting"], json!(true));
    assert_eq!(
        fixture
            .shell
            .exits
            .lock()
            .map(|exits| exits.clone())
            .unwrap_or_default(),
        vec![0],
        "壳层必须收到退出请求"
    );
    assert_error(
        fixture.state.dispatch("app_exit", json!({})),
        "missing_field",
        Some("confirm"),
        "app_exit/confirm",
    );
    // 重启。
    let value = fixture
        .state
        .dispatch("app_restart", json!({ "confirm": true }))
        .expect("app_restart 应成功");
    assert_eq!(value["restarting"], json!(true));
    assert_eq!(fixture.shell.restarts.load(Ordering::SeqCst), 1);
    // 文件选择（fixed picker 第一次返回路径、第二次取消）。
    let value = fixture
        .state
        .dispatch("ref_pick", json!({ "kind": "file" }))
        .expect("ref_pick 文件应成功");
    assert_eq!(
        value["path"].as_str().unwrap_or_default(),
        fixture.file.to_string_lossy()
    );
    let value = fixture
        .state
        .dispatch("ref_pick", json!({ "kind": "file" }))
        .expect("ref_pick 取消应成功");
    assert_eq!(value["path"], Value::Null);
    assert_error(
        fixture
            .state
            .dispatch("ref_pick", json!({ "kind": "bogus" })),
        "invalid_enum",
        None,
        "ref_pick/kind",
    );
    // 目录选择：固定 picker 第 3 次返回路径、第 4 次取消。
    let value = fixture
        .state
        .dispatch("startup_pick_target", Value::Null)
        .expect("startup_pick_target 应成功");
    assert_eq!(
        value["target_dir"].as_str().unwrap_or_default(),
        fixture.root.to_string_lossy()
    );
    let value = fixture
        .state
        .dispatch("startup_pick_target", json!({}))
        .expect("startup_pick_target 取消应成功");
    assert_eq!(value["target_dir"], Value::Null);
    // 路径校验仍由后续 artifact_add 复核（选择器只返回路径）。
    assert_error(
        fixture.state.dispatch(
            "artifact_add",
            json!({ "session_id": ULID, "path": fixture.missing.to_string_lossy() }),
        ),
        "artifact_path_rejected",
        None,
        "artifact_add/missing",
    );
}

#[test]
fn m9_04_messages_next_cursor_is_opaque_and_zero_progress_safe() {
    let session = ULID;
    let hash = cursor::filter_hash(&[cursor::STREAM_EVENTS, session, "500"]);
    let token = cursor::encode_cursor(cursor::STREAM_EVENTS, 7, &hash);
    let (pos, decoded) = cursor::decode_cursor(&token, cursor::STREAM_EVENTS).expect("可解析");
    assert_eq!(pos, 7);
    assert_eq!(decoded, hash);
    // 空页不产生游标（零推进）：编码辅助只在非空时调用。
    let (kept, truncated) = cursor::fit_budget(Vec::<Value>::new(), 65536).expect("空页");
    assert!(kept.is_empty() && !truncated);
    // 篡改 filter_hash 能被识别。
    let tampered = cursor::encode_cursor(cursor::STREAM_EVENTS, 7, "deadbeef");
    let (_, tampered_hash) = cursor::decode_cursor(&tampered, cursor::STREAM_EVENTS).expect("结构");
    assert_ne!(tampered_hash, hash);
}
