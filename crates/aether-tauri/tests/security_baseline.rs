//! 安全基线配置断言（M1-08 DoD 2 / DoD 4）。
//!
//! 从磁盘读取真实签入文件（`tauri.conf.json`、`capabilities/`、`Cargo.toml`），
//! 防止「源码改了、配置未改」。基线变更（CSP / capabilities / devtools）须走 ADR。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const EXPECTED_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src ipc: http://ipc.localhost; frame-src 'none'; object-src 'none'";

const CAPABILITY_FILE_ALLOWLIST: &[&str] = &["default.json"];
const CAPABILITY_ALLOWED_FIELDS: &[&str] = &[
    "$schema",
    "identifier",
    "description",
    "windows",
    "permissions",
];
const CAPABILITY_PERMISSION_ALLOWLIST: &[&str] = &[];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(path: &Path) -> Value {
    let raw =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("读取 {path:?} 失败：{error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("解析 {path:?} 失败：{error}"))
}

fn csp_directives(csp: &str) -> Vec<(String, Vec<String>)> {
    csp.split(';')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut tokens = part.split_whitespace();
            let name = tokens.next().unwrap_or_default().to_string();
            (name, tokens.map(str::to_string).collect())
        })
        .collect()
}

#[test]
fn csp_is_frozen_and_has_no_weak_sources() {
    let config = read_json(&manifest_dir().join("tauri.conf.json"));
    let csp = config["app"]["security"]["csp"]
        .as_str()
        .expect("app.security.csp 必须存在");
    assert_eq!(csp, EXPECTED_CSP, "CSP 基线变更须走 ADR");

    let directives = csp_directives(csp);
    let find = |directive: &str| -> Vec<String> {
        directives
            .iter()
            .find(|(name, _)| name == directive)
            .map(|(_, sources)| sources.clone())
            .unwrap_or_default()
    };

    assert_eq!(
        find("default-src"),
        vec!["'self'"],
        "default-src 仅允许 'self'"
    );
    assert_eq!(
        find("script-src"),
        vec!["'self'"],
        "script-src 仅允许 'self'"
    );
    assert_eq!(find("frame-src"), vec!["'none'"], "frame-src 必须为 'none'");
    assert_eq!(
        find("object-src"),
        vec!["'none'"],
        "object-src 必须为 'none'"
    );

    for (name, sources) in &directives {
        for source in sources {
            assert!(
                source != "*" && !source.contains("'unsafe-eval'"),
                "CSP 指令 {name} 含弱源 {source}"
            );
            let remote = source.starts_with("http://") || source.starts_with("https://");
            assert!(
                !remote || source == "http://ipc.localhost",
                "CSP 指令 {name} 含远程源 {source}"
            );
        }
    }
}

#[test]
fn with_global_tauri_is_disabled() {
    let config = read_json(&manifest_dir().join("tauri.conf.json"));
    assert_eq!(
        config["app"]["withGlobalTauri"],
        Value::Bool(false),
        "withGlobalTauri 必须为 false（设计 D7）"
    );
}

#[test]
fn asset_csp_modification_is_disabled() {
    // Tauri 默认注入 nonce/hash 以白名单化 HTML 内的脚本，等效放宽 script-src；
    // DoD1 要求内联/远程脚本被阻断，因此必须禁用（变更须走 ADR）。
    let config = read_json(&manifest_dir().join("tauri.conf.json"));
    assert_eq!(
        config["app"]["security"]["dangerousDisableAssetCspModification"],
        Value::Bool(true),
        "必须禁用 Tauri 资产 CSP 注入（dangerousDisableAssetCspModification: true）"
    );
}

#[test]
fn window_skeleton_matches_design() {
    let config = read_json(&manifest_dir().join("tauri.conf.json"));
    let windows = config["app"]["windows"]
        .as_array()
        .expect("app.windows 必须存在");
    assert_eq!(windows.len(), 1, "M1-08 仅允许单窗口");
    let main = &windows[0];
    assert_eq!(main["label"], "main");
    assert_eq!(main["width"], 1280);
    assert_eq!(main["height"], 800);
    assert_eq!(main["minWidth"], 960);
    assert_eq!(main["minHeight"], 640);
}

#[test]
fn capabilities_are_minimal_allowlist() {
    let dir = manifest_dir().join("capabilities");
    let mut files: Vec<String> = fs::read_dir(&dir)
        .expect("capabilities 目录必须存在")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    files.sort();
    assert_eq!(
        files, CAPABILITY_FILE_ALLOWLIST,
        "仅允许签入白名单内的 capability 文件；新增须评审"
    );

    for file in CAPABILITY_FILE_ALLOWLIST {
        let value = read_json(&dir.join(file));
        let object = value.as_object().expect("capability 必须是对象");
        for key in object.keys() {
            assert!(
                CAPABILITY_ALLOWED_FIELDS.contains(&key.as_str()),
                "capability {file} 出现未评审字段 {key}（remote/local/webviews 一律拒绝）"
            );
        }

        assert_eq!(value["identifier"], "default");
        assert_eq!(
            value["windows"],
            serde_json::json!(["main"]),
            "capabilities 必须按窗口裁剪"
        );
        assert_eq!(
            value["permissions"],
            serde_json::json!(CAPABILITY_PERMISSION_ALLOWLIST),
            "capabilities 权限集必须与签入的最小 allowlist 完全一致"
        );
    }
}

#[test]
fn devtools_are_debug_only() {
    let cargo_toml =
        fs::read_to_string(manifest_dir().join("Cargo.toml")).expect("读取 Cargo.toml");
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        assert!(
            !line.contains("devtools"),
            "Cargo.toml 不得启用 devtools 相关 feature：{line}"
        );
    }

    let config = read_json(&manifest_dir().join("tauri.conf.json"));
    for window in config["app"]["windows"]
        .as_array()
        .expect("app.windows 必须存在")
    {
        assert_ne!(
            window.get("devtools"),
            Some(&Value::Bool(true)),
            "窗口配置不得在 release 开启 devtools；devtools 仅 debug 可达"
        );
    }

    for source in ["src/lib.rs", "src/main.rs"] {
        let text = fs::read_to_string(manifest_dir().join(source)).expect("读取源码");
        assert!(
            !text.contains("open_devtools"),
            "{source} 不得调用 open_devtools"
        );
    }
}
