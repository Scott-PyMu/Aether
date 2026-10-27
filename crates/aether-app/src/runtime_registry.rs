//! M4-05/M7-01：官方运行时注册清单加载（安装产物内置只读清单）+ 逐条校验。
//!
//! 形态与 ADR-015 §2.2 决策 4 的注册清单 schema 对齐：
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
//! 解析基准（逻辑锚）：清单目录由运行期（当前可执行文件所在安装根的资源目录
//! `resource_dir()/runtime-bundle/`）解析；`program` 必须是纯相对路径，
//! canonicalize 后不得逃逸清单目录（换安装路径 / 便携形态 / 用户级安装共用同一
//! 逻辑锚；清单与程序均以 canonicalize 后的形态比较）。
//!
//! M7-01 逐条校验（ADR-015 决策 4）：
//! ① 严格 schema（未知字段/未知条目拒绝）；② 官方 id 白名单；③ program 路径约束；
//! ④ **构建期摘要比对**——期望摘要表由构建流水线（`build.rs`，数据源 = 打包前的
//! `runtime-bundle/`，可经 `AETHER_RUNTIME_BUNDLE_DIR` 覆盖）生成并编译进二进制，
//! 覆盖「清单规范化摘要」（键序固定 + 紧凑 JSON；覆盖 `args` 与清单字段篡改）与
//! 「每个 program 的 sha256」；任一失败 → 该条目拒绝加载并审计
//! （`runtime.registry_rejected`；原因分类 `missing_file` / `digest_mismatch` /
//! `unknown_id` / `path_rejected` / `schema_invalid`）。
//!
//! 缺失/不可解析 → 空注册表 + 告警，不阻塞应用启动；不支持进程内重扫（修复后重启）。
//! 用户可写清单一律不加载（清单本身不放可信摘要；无匹配的构建期期望即拒绝）。
//!
//! 测试构建（E2E/打包冒烟）经 `AETHER_RUNTIME_REGISTRY_DIR` 覆盖清单目录，并在
//! `cargo build` 时经 `AETHER_RUNTIME_BUNDLE_DIR` 让 build.rs 内嵌同一目录的期望摘要。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, OFFICIAL_RUNTIME_IDS};
use serde::Deserialize;

use crate::registry_identity;

/// 清单目录名（安装产物资源目录内；Tauri 迁移期与 `tauri.conf.json` `bundle.resources`
/// 对应，M9 后与 Electron `extraResources` 对应）。
pub const REGISTRY_DIR_NAME: &str = "runtime-bundle";
/// 清单文件名。
pub const REGISTRY_FILE_NAME: &str = "runtimes.json";
/// 支持的 schema 版本（ADR-015 §2.2#1）。
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;
/// 测试覆盖清单目录的环境变量（E2E / 打包冒烟）。
pub const REGISTRY_DIR_ENV: &str = "AETHER_RUNTIME_REGISTRY_DIR";
/// 构建期覆盖期望摘要数据源目录的环境变量（`build.rs` 消费；E2E 与默认目录同源）。
pub const REGISTRY_BUNDLE_DIR_ENV: &str = "AETHER_RUNTIME_BUNDLE_DIR";

/// 拒绝原因分类（ADR-015 §2.2#3；审计 detail 前缀）。
pub mod reject_reason {
    /// 清单/条目 schema 非法（含未知字段、schema_version 不匹配）。
    pub const SCHEMA_INVALID: &str = "schema_invalid";
    /// id 不命中官方白名单。
    pub const UNKNOWN_ID: &str = "unknown_id";
    /// 路径约束拒绝（绝对路径/父目录逃逸/逃逸清单目录）。
    pub const PATH_REJECTED: &str = "path_rejected";
    /// 程序文件缺失或非普通文件。
    pub const MISSING_FILE: &str = "missing_file";
    /// 摘要不匹配（清单规范化摘要 / program sha256 / 期望表缺失）。
    pub const DIGEST_MISMATCH: &str = "digest_mismatch";
}

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

/// 构建期期望摘要表（编译进二进制；ADR-015 §2.2#2）。
///
/// 由 `build.rs` 生成 `registry_expectation.rs`（`OUT_DIR`）；运行期经
/// [`ExpectedRegistry::compiled`] 取用；测试经 [`ExpectedRegistry::compute_from_dir`]
/// 或 [`ExpectedRegistry::from_parts`] 注入（DI 口径见 `m7_01_registry` 集成测试）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExpectedRegistry {
    /// 本构建是否内嵌了期望摘要（无 `runtime-bundle` 时为 `false`；此时清单一律拒绝）。
    pub present: bool,
    /// 清单规范化形态 sha256（`sha256:<hex>`）。
    pub manifest_digest: Option<String>,
    /// program（清单内相对路径原样）→ sha256（`sha256:<hex>`）。
    pub program_digests: Vec<(String, String)>,
}

impl ExpectedRegistry {
    /// 本构建内嵌的期望摘要（`build.rs` 生成）。
    pub fn compiled() -> Self {
        mod generated {
            include!(concat!(env!("OUT_DIR"), "/registry_expectation.rs"));
        }
        Self {
            present: generated::PRESENT,
            manifest_digest: if generated::PRESENT {
                Some(generated::MANIFEST_DIGEST.to_owned())
            } else {
                None
            },
            program_digests: generated::PROGRAM_DIGESTS
                .iter()
                .map(|(program, digest)| ((*program).to_owned(), (*digest).to_owned()))
                .collect(),
        }
    }

    /// 显式构造（测试注入；生产恒使用 [`ExpectedRegistry::compiled`]）。
    pub fn from_parts(
        present: bool,
        manifest_digest: Option<String>,
        program_digests: Vec<(String, String)>,
    ) -> Self {
        Self {
            present,
            manifest_digest,
            program_digests,
        }
    }

    /// 从清单目录计算期望（与 `build.rs` 同口径；**测试/构建辅助**，运行期不得使用）。
    ///
    /// 返回 `None` = 清单缺失/不可解析/程序缺失（调用方按各自语义处置）。
    pub fn compute_from_dir(dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(dir.join(REGISTRY_FILE_NAME)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let document: RegistryDocument = serde_json::from_value(value.clone()).ok()?;
        let mut program_digests = Vec::new();
        for entry in &document.runtimes {
            let digest = registry_identity::program_digest(&dir.join(&entry.program)).ok()?;
            program_digests.push((entry.program.clone(), digest));
        }
        Some(Self {
            present: true,
            manifest_digest: Some(registry_identity::manifest_digest(&value)),
            program_digests,
        })
    }
}

/// 加载结果（诊断/证据用）。
#[derive(Debug, Default)]
pub struct RegistryLoadReport {
    /// 清单目录（None = 未配置/资源目录不可用）。
    pub dir: Option<PathBuf>,
    /// 合法条目构造的运行时规格。
    pub specs: Vec<RuntimeSpec>,
    /// 被拒条目（id → 原因分类 + 细节；不阻塞启动）。
    pub rejected: Vec<(String, String)>,
    /// 清单文件是否存在。
    pub manifest_present: bool,
    /// 本构建是否内嵌期望摘要（诊断：false 时清单条目一律拒绝）。
    pub expectation_present: bool,
}

/// 解析清单目录：`AETHER_RUNTIME_REGISTRY_DIR` 覆盖 → 壳层资源目录（`resource_dir/runtime-bundle`）。
///
/// 壳层无关口径（ADR-029 D-029-2）：资源目录由调用方（`aether-tauri` / daemon）解析后
/// 传入；应用层不感知 Tauri/Electron 的路径 API。
pub fn resolve_registry_dir(resource_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(REGISTRY_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    resource_dir.map(|root| root.join(REGISTRY_DIR_NAME))
}

/// 加载注册清单并构造运行时规格（期望摘要 = 本构建内嵌；生产入口）。
pub fn load_registry(dir: &Path) -> RegistryLoadReport {
    load_registry_with_expected(dir, &ExpectedRegistry::compiled())
}

/// 加载注册清单并构造运行时规格（显式期望摘要；测试注入口径）。
///
/// 失败条目跳过 + 原因分类；清单级失败（不可解析/规范化摘要不匹配/期望缺失）
/// 时全部条目拒绝（不返回任何 spec）。
pub fn load_registry_with_expected(dir: &Path, expected: &ExpectedRegistry) -> RegistryLoadReport {
    let manifest_path = dir.join(REGISTRY_FILE_NAME);
    let mut report = RegistryLoadReport {
        dir: Some(dir.to_path_buf()),
        expectation_present: expected.present,
        ..RegistryLoadReport::default()
    };
    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(_) => return report,
    };
    report.manifest_present = true;
    // ① 严格 schema（先解析 `Value` 供规范化摘要；再严格结构校验）。
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            report.rejected.push((
                "<document>".to_owned(),
                format!("{}: {error}", reject_reason::SCHEMA_INVALID),
            ));
            return report;
        }
    };
    let document: RegistryDocument = match serde_json::from_value(value.clone()) {
        Ok(document) => document,
        Err(error) => {
            report.rejected.push((
                "<document>".to_owned(),
                format!("{}: {error}", reject_reason::SCHEMA_INVALID),
            ));
            return report;
        }
    };
    if document.schema_version != REGISTRY_SCHEMA_VERSION {
        report.rejected.push((
            "<document>".to_owned(),
            format!(
                "{}: schema_version={}",
                reject_reason::SCHEMA_INVALID,
                document.schema_version
            ),
        ));
        return report;
    }
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(canonical) => canonical,
        Err(error) => {
            report.rejected.push((
                "<document>".to_owned(),
                format!("{}: {error}", reject_reason::PATH_REJECTED),
            ));
            return report;
        }
    };
    // ④ 清单规范化摘要（覆盖 `args` 与清单字段篡改）；期望缺失/不匹配 → 全部条目拒绝。
    if !expected.present {
        for entry in &document.runtimes {
            report.rejected.push((
                entry.id.clone(),
                format!(
                    "{}: 构建期期望摘要缺失（本构建未内嵌 runtime-bundle 摘要；ADR-015 §2.2#2）",
                    reject_reason::DIGEST_MISMATCH
                ),
            ));
        }
        return report;
    }
    let actual_manifest_digest = registry_identity::manifest_digest(&value);
    if expected.manifest_digest.as_deref() != Some(actual_manifest_digest.as_str()) {
        for entry in &document.runtimes {
            report.rejected.push((
                entry.id.clone(),
                format!(
                    "{}: 清单规范化摘要不匹配（期望 {}，实际 {actual_manifest_digest}）",
                    reject_reason::DIGEST_MISMATCH,
                    expected.manifest_digest.as_deref().unwrap_or("-")
                ),
            ));
        }
        return report;
    }
    let digest_index: HashMap<&str, &str> = expected
        .program_digests
        .iter()
        .map(|(program, digest)| (program.as_str(), digest.as_str()))
        .collect();
    for entry in document.runtimes {
        match validate_entry(&entry, &canonical_dir, &digest_index) {
            Ok(spec) => report.specs.push(spec),
            Err(reason) => report.rejected.push((entry.id, reason)),
        }
    }
    report
}

fn validate_entry(
    entry: &RegistryEntry,
    canonical_dir: &Path,
    digest_index: &HashMap<&str, &str>,
) -> Result<RuntimeSpec, String> {
    // ② 官方 id 白名单（Mock 仅测试构建；`pi`/`hermes` 为白名单占位，无清单条目）。
    if !OFFICIAL_RUNTIME_IDS.contains(&entry.id.as_str()) {
        return Err(format!("{}: id={}", reject_reason::UNKNOWN_ID, entry.id));
    }
    if entry.id.trim().is_empty() || entry.name.trim().is_empty() {
        return Err(format!("{}: 空 id/name", reject_reason::SCHEMA_INVALID));
    }
    // ③ program 路径：纯相对路径（禁绝对/前缀/RootDir/ParentDir），canonicalize 无逃逸。
    let program = Path::new(&entry.program);
    if entry.program.trim().is_empty()
        || !program
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "{}: 非纯相对路径（{}）",
            reject_reason::PATH_REJECTED,
            entry.program
        ));
    }
    let resolved = canonical_dir.join(program);
    let canonical_program = match std::fs::canonicalize(&resolved) {
        Ok(canonical) => canonical,
        Err(error) => return Err(format!("{}: {error}", reject_reason::MISSING_FILE)),
    };
    if !canonical_program.starts_with(canonical_dir) {
        return Err(format!("{}: 逃逸清单目录", reject_reason::PATH_REJECTED));
    }
    if !canonical_program.is_file() {
        return Err(format!("{}: 非普通文件", reject_reason::MISSING_FILE));
    }
    // ④ program sha256（构建期期望表；逐条比对）。
    let expected_digest = match digest_index.get(entry.program.as_str()) {
        Some(digest) => *digest,
        None => {
            return Err(format!(
                "{}: 期望摘要表无 program 条目（{}）",
                reject_reason::DIGEST_MISMATCH,
                entry.program
            ))
        }
    };
    let actual_digest = match registry_identity::program_digest(&canonical_program) {
        Ok(digest) => digest,
        Err(error) => return Err(format!("{}: {error}", reject_reason::MISSING_FILE)),
    };
    if actual_digest != expected_digest {
        return Err(format!(
            "{}: program sha256 不匹配（期望 {expected_digest}，实际 {actual_digest}）",
            reject_reason::DIGEST_MISMATCH
        ));
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
            "aether-m7-01-registry-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录");
        dir
    }

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join(REGISTRY_FILE_NAME), body).expect("写清单");
    }

    fn manifest_body(program: &str, args: &str) -> String {
        format!(
            r#"{{"schema_version":1,"runtimes":[{{"id":"claude-code","name":"Claude Code","kind":"claude-code","version":"0.1.0","protocol":"1.0","program":"{program}","args":[{args}]}}]}}"#
        )
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
    fn valid_entries_load_as_official_specs_with_matching_digests() {
        let dir = temp_dir("valid");
        let program = dir.join("aether-claude-adapter.exe");
        std::fs::write(&program, b"stub").expect("程序桩");
        write_manifest(&dir, &manifest_body("aether-claude-adapter.exe", "\"--x\""));
        let expected = ExpectedRegistry::compute_from_dir(&dir).expect("期望摘要");
        let report = load_registry_with_expected(&dir, &expected);
        assert_eq!(report.specs.len(), 1);
        assert!(report.rejected.is_empty());
        assert_eq!(report.specs[0].manifest.id, "claude-code");
        assert!(report.specs[0].manifest.official);
        assert_eq!(report.specs[0].manifest.args, vec!["--x".to_owned()]);
        assert!(report.expectation_present);
    }

    /// DoD3：无构建期期望摘要（本构建未内嵌 bundle）→ 清单一律拒绝（不静默放行）。
    #[test]
    fn manifest_without_compiled_expectation_is_rejected() {
        let dir = temp_dir("no-expectation");
        std::fs::write(dir.join("aether-claude-adapter.exe"), b"stub").expect("程序桩");
        write_manifest(&dir, &manifest_body("aether-claude-adapter.exe", ""));
        let report = load_registry_with_expected(&dir, &ExpectedRegistry::default());
        assert!(report.specs.is_empty());
        assert_eq!(report.rejected.len(), 1);
        assert!(report.rejected[0].1.starts_with(reject_reason::DIGEST_MISMATCH));
        assert!(!report.expectation_present);
    }

    /// DoD3：program 篡改（摘要不匹配）→ 拒绝；再次修复后同条目恢复加载（重启重扫口径）。
    #[test]
    fn tampered_program_is_rejected_by_digest() {
        let dir = temp_dir("tamper-program");
        let program = dir.join("aether-claude-adapter.exe");
        std::fs::write(&program, b"pristine").expect("程序");
        write_manifest(&dir, &manifest_body("aether-claude-adapter.exe", ""));
        let expected = ExpectedRegistry::compute_from_dir(&dir).expect("期望摘要");
        // 篡改 program（同长度换字节 → sha256 变化）。
        std::fs::write(&program, b"evilpwn!").expect("篡改");
        let report = load_registry_with_expected(&dir, &expected);
        assert!(report.specs.is_empty());
        assert_eq!(report.rejected.len(), 1);
        assert!(
            report.rejected[0]
                .1
                .starts_with(reject_reason::DIGEST_MISMATCH),
            "{}",
            report.rejected[0].1
        );
    }

    /// DoD3：清单字段/`args` 篡改（规范化摘要覆盖）→ 全部条目拒绝。
    #[test]
    fn tampered_manifest_fields_are_rejected_by_canonical_digest() {
        let dir = temp_dir("tamper-manifest");
        std::fs::write(dir.join("aether-claude-adapter.exe"), b"stub").expect("程序桩");
        write_manifest(&dir, &manifest_body("aether-claude-adapter.exe", ""));
        let expected = ExpectedRegistry::compute_from_dir(&dir).expect("期望摘要");
        // args 篡改（内容级变化；仅格式/空白差异不影响规范化摘要）。
        write_manifest(&dir, &manifest_body("aether-claude-adapter.exe", "\"--evil\""));
        let report = load_registry_with_expected(&dir, &expected);
        assert!(report.specs.is_empty());
        assert_eq!(report.rejected.len(), 1);
        assert!(report.rejected[0].1.starts_with(reject_reason::DIGEST_MISMATCH));
        // 仅格式差异（换行/缩进）不改变规范化摘要 → 仍可加载。
        write_manifest(
            &dir,
            &manifest_body("aether-claude-adapter.exe", "").replace(",", ",\n  "),
        );
        let report = load_registry_with_expected(&dir, &expected);
        assert_eq!(report.specs.len(), 1, "空白/格式差异不应触发拒绝");
        assert!(report.rejected.is_empty());
    }

    #[test]
    fn unknown_id_absolute_and_escape_paths_are_rejected() {
        let dir = temp_dir("reject");
        let outside = dir.parent().expect("父目录").join("aether-m7-01-outside");
        std::fs::create_dir_all(&outside).expect("外部目录");
        std::fs::write(outside.join("evil.exe"), b"stub").expect("外部程序");
        write_manifest(
            &dir,
            r#"{"schema_version":1,"runtimes":[
              {"id":"third-party","name":"X","kind":"x","version":"1","protocol":"1.0","program":"x.exe"},
              {"id":"codex","name":"Y","kind":"codex","version":"1","protocol":"1.0","program":"C:\\Windows\\evil.exe"},
              {"id":"deepseek-harness","name":"Z","kind":"dsh","version":"1","protocol":"1.0","program":"../aether-m7-01-outside/evil.exe"}
            ]}"#,
        );
        let expected = ExpectedRegistry::from_parts(
            true,
            Some(registry_identity::manifest_digest(
                &serde_json::from_str::<serde_json::Value>(
                    &std::fs::read_to_string(dir.join(REGISTRY_FILE_NAME)).expect("清单"),
                )
                .expect("解析"),
            )),
            Vec::new(),
        );
        let report = load_registry_with_expected(&dir, &expected);
        assert!(report.specs.is_empty());
        assert_eq!(report.rejected.len(), 3);
        assert!(report.rejected[0].1.starts_with(reject_reason::UNKNOWN_ID));
        assert!(report.rejected[1].1.starts_with(reject_reason::PATH_REJECTED));
        assert!(report.rejected[2].1.starts_with(reject_reason::PATH_REJECTED));
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

    /// DoD3 误杀样本：路径含空格/非 ASCII、换安装路径（便携形态）、用户级安装
    /// （临时目录）、Windows 8.3 短名共用同一逻辑锚解析（canonicalize 后比对）。
    #[test]
    fn path_samples_with_spaces_non_ascii_and_short_names_are_not_false_rejected() {
        let base = std::env::temp_dir().join(format!("aether m7-01 便携 {}", std::process::id()));
        let dir = base.join("runtime-bundle");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&dir).expect("便携目录");
        let program = dir.join("aether claude 适配器.exe");
        std::fs::write(&program, b"stub").expect("程序桩");
        write_manifest(&dir, &manifest_body("aether claude 适配器.exe", ""));
        let expected = ExpectedRegistry::compute_from_dir(&dir).expect("期望摘要");
        let report = load_registry_with_expected(&dir, &expected);
        assert_eq!(report.specs.len(), 1, "含空格/非 ASCII 路径不得误杀");
        assert!(report.rejected.is_empty());
        // 8.3 短名形态：目录自身经短名重进（Windows）；canonicalize 双向归一。
        #[cfg(windows)]
        {
            let short = short_name(&dir);
            if let Some(short_dir) = short {
                let report = load_registry_with_expected(&short_dir, &expected);
                assert_eq!(report.specs.len(), 1, "8.3 短名形态不得误杀：{short_dir:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 经 `dir /x` 取目录 8.3 短名（仅 Windows；不可得返回 `None`）。
    #[cfg(windows)]
    fn short_name(dir: &Path) -> Option<PathBuf> {
        let parent = dir.parent()?;
        let name = dir.file_name()?.to_string_lossy().to_string();
        let output = std::process::Command::new("cmd")
            .args(["/C", "dir", "/x", "/b"])
            .current_dir(parent)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        // 优先找含 `~` 的替代名；找不到时回退原名目录。
        let short = text.lines().find(|line| line.contains('~'))?.trim();
        if short.is_empty() {
            return None;
        }
        let candidate = parent.join(short);
        if candidate.exists() {
            Some(candidate)
        } else {
            let _ = name;
            None
        }
    }
}
