//! M9-10 DoD-5：路径参数命令全集（P1-8）安全矩阵。
//!
//! 断言两件事：
//! 1. `packages/protocol/src/path-commands.json`（由 `aether-app` DTO 生成）
//!    覆盖 P1-8 必有命令，且集合与已复核全集一致；
//! 2. 每条路径命令在**到达后端之前**拒绝相对路径（canonicalize/白名单/形态校验），
//!    返回结构化 `path_rejected` / `artifact_path_rejected`——不存在「解析通过再补校验」
//!    的旁路。
//!
//! 可写性与空间护栏由后端执行（`SpaceProbe::ensure_writable` +
//! `required_space_bytes`；`pnpm verify:security` 静态断言），本测试只锁命令层。
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use aether_app::ipc::backend::IpcBackend;
use aether_app::ipc::{IpcError, IpcState};
use aether_app::startup::detect::DetectionContext;
use aether_app::startup::{DataDirSource, StartupGate};

const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// 全部方法默认 `not_implemented`；路径校验必须先于后端调用失败。
struct NoopBackend;

impl IpcBackend for NoopBackend {}

fn fixture(label: &str) -> (IpcState, PathBuf) {
    let unique = format!(
        "aether-m9-10-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    );
    let data_dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&data_dir).expect("创建临时数据目录");
    let gate = StartupGate::bootstrap_at(
        data_dir.clone(),
        DataDirSource::Default,
        DetectionContext::native(),
        None,
    );
    let state = IpcState::with_startup(Arc::new(NoopBackend), Vec::new(), Arc::new(gate));
    (state, data_dir)
}

fn error_code(result: Result<Value, IpcError>) -> String {
    match result {
        Ok(value) => panic!("路径校验未拒绝，返回：{value}"),
        Err(error) => error.code.as_str().to_owned(),
    }
}

fn assert_rejected(state: &IpcState, command: &str, payload: Value, expected: &str) {
    let code = error_code(state.dispatch(command, payload));
    assert_eq!(code, expected, "命令 {command} 的拒绝码不符");
}

/// 生成物与 DTO 集合（DoD-5「清单从 DTO 生成，CI 断言集合」）。
#[test]
fn path_command_manifest_covers_p1_8_and_reviewed_set() {
    const MANIFEST: &str = include_str!("../../../packages/protocol/src/path-commands.json");
    let model: Value = serde_json::from_str(MANIFEST).expect("path-commands.json 必须是合法 JSON");
    assert_eq!(model["schema"], "aether.path-commands.v1");
    assert_eq!(model["counts"]["commands"], 10);
    assert_eq!(model["counts"]["required"], 8);

    let names: Vec<String> = model["commands"]
        .as_array()
        .expect("commands 必须是数组")
        .iter()
        .map(|entry| {
            assert!(
                entry["path_fields"]
                    .as_array()
                    .is_some_and(|fields| !fields.is_empty()),
                "{} 缺少 path_fields",
                entry["name"]
            );
            assert!(
                entry["validator"].as_str().is_some_and(|value| !value.is_empty()),
                "{} 缺少 validator",
                entry["name"]
            );
            entry["name"].as_str().expect("name 必须是字符串").to_owned()
        })
        .collect();

    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "archive_export",
            "artifact_add",
            "audit_export",
            "backup_create",
            "backup_restore",
            "export_diagnostics",
            "session_export",
            "startup_migrate",
            "workspace_create",
            "workspace_set",
        ],
        "路径命令全集漂移（新增路径命令须评审并更新生成器）"
    );

    let required: Vec<&str> = model["required"]
        .as_array()
        .expect("required 必须是数组")
        .iter()
        .map(|name| name.as_str().expect("required 元素必须是字符串"))
        .collect();
    for command in [
        "artifact_add",
        "workspace_set",
        "startup_migrate",
        "backup_create",
        "backup_restore",
        "export_diagnostics",
        "session_export",
        "archive_export",
    ] {
        assert!(required.contains(&command), "P1-8 必有命令缺失：{command}");
        assert!(names.iter().any(|name| name == command), "生成清单缺失：{command}");
    }
}

/// 每条路径命令：相对路径必须在命令层被拒绝（后端默认 not_implemented 不得出现）。
#[test]
fn every_path_command_rejects_relative_path_before_backend() {
    let (state, data_dir) = fixture("relative");
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "artifact_add",
            json!({ "session_id": ULID, "path": "relative/file.txt" }),
            "artifact_path_rejected",
        ),
        (
            "workspace_set",
            json!({ "root_path": "relative/workspace" }),
            "path_rejected",
        ),
        (
            "workspace_create",
            json!({ "name": "ws", "root_path": "relative/workspace" }),
            "path_rejected",
        ),
        (
            "startup_migrate",
            json!({ "target_dir": "relative/target" }),
            "path_rejected",
        ),
        (
            "backup_create",
            json!({ "target_dir": "relative/backup" }),
            "path_rejected",
        ),
        (
            "backup_restore",
            json!({ "source": { "external": { "path": "relative/backup.db" } } }),
            "path_rejected",
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": "relative/diagnostics" }),
            "path_rejected",
        ),
        (
            "session_export",
            json!({ "session_id": ULID, "target_dir": "relative/export" }),
            "path_rejected",
        ),
        (
            "archive_export",
            json!({ "target_dir": "relative/archive" }),
            "path_rejected",
        ),
        (
            "audit_export",
            json!({ "target_dir": "relative/audit" }),
            "path_rejected",
        ),
    ];
    for (command, payload, expected) in cases {
        assert_rejected(&state, command, payload, expected);
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 绝对但不存在 / NUL / 超长：canonicalize 与形态校验必须拒绝。
#[test]
fn path_commands_reject_missing_nul_and_overlong_paths() {
    let (state, data_dir) = fixture("shape");
    let missing = data_dir.join("does-not-exist").join("child");
    let missing_text = missing.to_string_lossy().into_owned();
    let overlong = format!("{}{}", data_dir.to_string_lossy(), "x".repeat(5000));

    for (command, payload) in [
        (
            "workspace_set",
            json!({ "root_path": missing.to_string_lossy() }),
        ),
        (
            "export_diagnostics",
            json!({ "target_dir": missing_text }),
        ),
        (
            "backup_create",
            json!({ "target_dir": "C:\\bad\u{0000}path" }),
        ),
        (
            "session_export",
            json!({ "session_id": ULID, "target_dir": overlong }),
        ),
    ] {
        assert_rejected(&state, command, payload, "path_rejected");
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 备份/导出/归档后端的空间护栏（静态锚点）：确保 DoD-5 的「空间护栏」不是只写在文档里。
#[test]
fn backup_and_export_controls_keep_space_guards() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for (relative, needle) in [
        ("src/backup_control.rs", "ensure_writable"),
        ("src/backup_auto.rs", "required_space_bytes"),
        ("src/backup_auto.rs", "ensure_writable"),
        ("src/archive_control.rs", "ensure_writable"),
        ("src/archive_control.rs", "required_space_bytes"),
    ] {
        let text = std::fs::read_to_string(root.join(relative)).expect("读取源文件");
        assert!(text.contains(needle), "{relative} 缺少空间护栏锚点 {needle}");
    }
}
