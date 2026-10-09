//! M4-05：官方运行时注册清单加载（安装产物内置只读清单）。
//!
//! 形态与 ADR-015 §2.2 决策 4 的注册清单 schema 对齐（P1 M7-01 的逐条校验在
//! 本模块之上实现；本模块为 P0 安装产物注册的生产路径 + M7-01 的接口一致性义务）：
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "runtimes": [
//!     { "id": "claude-code", "name": "Claude Code", "kind": "claude-code",
//!       "version": "0.1.0", "protocol": "1.0",
//!       "program": "aether-claude-adapter.exe", "args": [] }
//!   ]
//! }
//! ```
//!
//! 解析基准（逻辑锚）：清单与 `program` 同目录；`program` 必须是相对路径，
//! canonicalize 后不得逃逸清单目录（ADR-015 §2.2#1 的 P0 子集；构建期摘要比对
//! 与官方 id 白名单强化校验随 M7-01 落地）。缺失/不可解析 → 空注册表 + 告警，
//! 不阻塞应用启动（D5：启用偏好与注册校验不阻塞启动的既有口径）。
//!
//! 测试构建（E2E/打包冒烟）经 `AETHER_RUNTIME_REGISTRY_DIR` 覆盖清单目录；
//! 生产从安装产物资源目录 `resource_dir()/runtime-bundle/` 解析。

use std::path::{Component, Path, PathBuf};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, OFFICIAL_RUNTIME_IDS};
use serde::Deserialize;
use tauri::{AppHandle, Manager};

/// 清单目录名（安装产物资源目录内；与 `tauri.conf.json` `bundle.resources` 对应）。
pub const REGISTRY_DIR_NAME: &str = "runtime-bundle";
/// 清单文件名。
pub const REGISTRY_FILE_NAME: &str = "runtimes.json";
/// 支持的 schema 版本（ADR-015 §2.2#1）。
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;
/// 测试覆盖清单目录的环境变量（E2E / 打包冒烟）。
pub const REGISTRY_DIR_ENV: &str = "AETHER_RUNTIME_REGISTRY_DIR";

/// 注册清单文档。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    schema_version: u32,
    runtimes: Vec<RegistryEntry>,
}

/// 注册清单条目（ADR-015 §2.2#1；不含密钥与 `api_key_ref`）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEntry {
    id: String,
    name: String,
    kind: String,
    version: String,
    protocol: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

/// 加载结果（诊断/证据用）。
#[derive(Debug, Default)]
pub struct RegistryLoadReport {
    /// 清单目录（None = 未配置/资源目录不可用）。
    pub dir: Option<PathBuf>,
    /// 合法条目构造的运行时规格。
    pub specs: Vec<RuntimeSpec>,
    /// 被拒条目（id → 原因分类；不阻塞启动）。
    pub rejected: Vec<(String, String)>,
    /// 清单文件是否存在。
    pub manifest_present: bool,
}

/// 解析清单目录：`AETHER_RUNTIME_REGISTRY_DIR` 覆盖 → 安装产物资源目录。
pub fn resolve_registry_dir(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(REGISTRY_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    app.path()
        .resource_dir()
        .ok()
        .map(|root| root.join(REGISTRY_DIR_NAME))
}

/// 加载注册清单并构造运行时规格（失败条目跳过 + 原因分类）。
pub fn load_registry(dir: &Path) -> RegistryLoadReport {
    let manifest_path = dir.join(REGISTRY_FILE_NAME);
    let mut report = RegistryLoadReport {
        dir: Some(dir.to_path_buf()),
        ..RegistryLoadReport::default()
    };
    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(_) => return report,
    };
    report.manifest_present = true;
    let document: RegistryDocument = match serde_json::from_str(&raw) {
        Ok(document) => document,
        Err(error) => {
            report
                .rejected
                .push(("<document>".to_owned(), format!("schema_invalid: {error}")));
            return report;
        }
    };
    if document.schema_version != REGISTRY_SCHEMA_VERSION {
        report.rejected.push((
            "<document>".to_owned(),
            format!("schema_invalid: schema_version={}", document.schema_version),
        ));
        return report;
    }
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(canonical) => canonical,
        Err(error) => {
            report
                .rejected
                .push(("<document>".to_owned(), format!("path_rejected: {error}")));
            return report;
        }
    };
    for entry in document.runtimes {
        match validate_entry(&entry, &canonical_dir) {
            Ok(spec) => report.specs.push(spec),
            Err(reason) => report.rejected.push((entry.id, reason)),
        }
    }
    report
}

fn validate_entry(entry: &RegistryEntry, canonical_dir: &Path) -> Result<RuntimeSpec, String> {
    if !OFFICIAL_RUNTIME_IDS.contains(&entry.id.as_str()) {
        return Err("unknown_id".to_owned());
    }
    if entry.id.trim().is_empty() || entry.name.trim().is_empty() {
        return Err("schema_invalid: 空 id/name".to_owned());
    }
    let program = Path::new(&entry.program);
    if program.is_absolute() {
        return Err("path_rejected: 禁止绝对路径".to_owned());
    }
    if program
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("path_rejected: 禁止父目录逃逸".to_owned());
    }
    let resolved = canonical_dir.join(program);
    let canonical_program = match std::fs::canonicalize(&resolved) {
        Ok(canonical) => canonical,
        Err(error) => return Err(format!("missing_file: {error}")),
    };
    if !canonical_program.starts_with(canonical_dir) {
        return Err("path_rejected: 逃逸清单目录".to_owned());
    }
    if !canonical_program.is_file() {
        return Err("missing_file: 非普通文件".to_owned());
    }
    let manifest = RuntimeManifest {
        id: entry.id.clone(),
        name: entry.name.clone(),
        kind: entry.kind.clone(),
        version: entry.version.clone(),
        protocol: entry.protocol.clone(),
        official: true,
        program: canonical_program,
        args: entry.args.clone(),
        env: Vec::new(),
        enabled: true,
    };
    Ok(RuntimeSpec::with_fresh_token(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aether-m4-05-registry-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录");
        dir
    }

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join(REGISTRY_FILE_NAME), body).expect("写清单");
    }

    #[test]
    fn missing_manifest_is_empty_registry_without_error() {
        let dir = temp_dir("missing");
        let report = load_registry(&dir);
        assert!(!report.manifest_present);
        assert!(report.specs.is_empty());
        assert!(report.rejected.is_empty());
    }

    #[test]
    fn valid_entries_load_as_official_specs() {
        let dir = temp_dir("valid");
        let program = dir.join("aether-claude-adapter.exe");
        std::fs::write(&program, b"stub").expect("程序桩");
        write_manifest(
            &dir,
            r#"{"schema_version":1,"runtimes":[{"id":"claude-code","name":"Claude Code","kind":"claude-code","version":"0.1.0","protocol":"1.0","program":"aether-claude-adapter.exe","args":["--x"]}]}"#,
        );
        let report = load_registry(&dir);
        assert_eq!(report.specs.len(), 1);
        assert!(report.rejected.is_empty());
        assert_eq!(report.specs[0].manifest.id, "claude-code");
        assert!(report.specs[0].manifest.official);
        assert_eq!(report.specs[0].manifest.args, vec!["--x".to_owned()]);
    }

    #[test]
    fn unknown_id_absolute_and_escape_paths_are_rejected() {
        let dir = temp_dir("reject");
        let outside = dir.parent().expect("父目录").join("aether-m4-05-outside");
        std::fs::create_dir_all(&outside).expect("外部目录");
        std::fs::write(outside.join("evil.exe"), b"stub").expect("外部程序");
        write_manifest(
            &dir,
            r#"{"schema_version":1,"runtimes":[
              {"id":"third-party","name":"X","kind":"x","version":"1","protocol":"1.0","program":"x.exe"},
              {"id":"codex","name":"Y","kind":"codex","version":"1","protocol":"1.0","program":"C:\\Windows\\evil.exe"},
              {"id":"deepseek-harness","name":"Z","kind":"dsh","version":"1","protocol":"1.0","program":"../aether-m4-05-outside/evil.exe"}
            ]}"#,
        );
        let report = load_registry(&dir);
        assert!(report.specs.is_empty());
        assert_eq!(report.rejected.len(), 3);
        assert!(report.rejected[0].1.starts_with("unknown_id"));
        assert!(report.rejected[1].1.starts_with("path_rejected"));
        assert!(report.rejected[2].1.starts_with("path_rejected"));
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn schema_version_and_unknown_fields_are_rejected() {
        let dir = temp_dir("schema");
        write_manifest(&dir, r#"{"schema_version":2,"runtimes":[]}"#);
        assert_eq!(load_registry(&dir).rejected.len(), 1);
        write_manifest(&dir, r#"{"schema_version":1,"runtimes":[],"extra":true}"#);
        assert_eq!(load_registry(&dir).rejected.len(), 1);
    }
}
