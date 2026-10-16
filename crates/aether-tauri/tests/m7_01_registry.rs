//! M7-01 DoD3：注册清单逐条校验（严格 schema / 官方 id 白名单 / program 路径 /
//! 构建期摘要比对）与误杀样本（ADR-015 决策 4；含 M4-05 注册产物接口一致性）。
//!
//! 两个层级：
//! - 常规用例经 [`ExpectedRegistry`] 显式注入期望（DI 口径；临时目录自洽构造）；
//! - `compiled_expectation_matches_real_bundle` 为 `#[ignore]`，由
//!   `scripts/test/m7-01/verify-m7-01.mjs` 构建真实运行时包后，以
//!   `AETHER_RUNTIME_BUNDLE_DIR=<bundle>`（build.rs 内嵌同源期望）与
//!   `AETHER_M7_01_BUNDLE_DIR=<bundle>` 运行（`load_registry` 走编译进二进制的信任锚）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use aether_tauri::registry_identity::manifest_digest;
use aether_tauri::runtime_registry::{
    load_registry, load_registry_with_expected, ExpectedRegistry, REGISTRY_FILE_NAME,
};
use serde_json::{json, Value};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aether-m7-01-it-registry-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("临时目录");
    dir
}

fn entry(id: &str, program: &str, args: Value) -> Value {
    json!({
        "id": id,
        "name": format!("{id} 显示名"),
        "kind": id,
        "version": "0.1.0",
        "protocol": "1.0",
        "program": program,
        "args": args,
    })
}

fn write_manifest(dir: &Path, entries: &[Value]) -> Value {
    let document = json!({ "schema_version": 1, "runtimes": entries });
    std::fs::write(
        dir.join(REGISTRY_FILE_NAME),
        serde_json::to_string_pretty(&document).expect("序列化"),
    )
    .expect("写清单");
    document
}

#[test]
fn strict_schema_unknown_id_and_path_escape_are_rejected_with_reason_classes() {
    let dir = temp_dir("schema");
    let outside = dir.parent().expect("父目录").join("aether-m7-01-it-outside");
    std::fs::create_dir_all(&outside).expect("外部目录");
    std::fs::write(outside.join("evil.exe"), b"stub").expect("外部程序");
    std::fs::write(dir.join("ok.exe"), b"stub").expect("程序桩");
    let document = write_manifest(
        &dir,
        &[
            entry("third-party", "ok.exe", json!([])),
            entry("codex", "C:\\Windows\\evil.exe", json!([])),
            entry("deepseek-harness", "../aether-m7-01-it-outside/evil.exe", json!([])),
            entry("claude-code", "/abs/ok.exe", json!([])),
        ],
    );
    let expected = ExpectedRegistry::from_parts(
        true,
        Some(manifest_digest(&document)),
        vec![("ok.exe".to_owned(), "sha256:anything".to_owned())],
    );
    let report = load_registry_with_expected(&dir, &expected);
    assert!(report.specs.is_empty());
    assert_eq!(report.rejected.len(), 4);
    assert!(report.rejected[0].1.starts_with("unknown_id"));
    assert!(report.rejected[1].1.starts_with("path_rejected"));
    assert!(report.rejected[2].1.starts_with("path_rejected"));
    assert!(report.rejected[3].1.starts_with("path_rejected"));

    // 严格 schema：未知字段 / 未知 schema_version / 顶层未知字段。
    std::fs::write(
        dir.join(REGISTRY_FILE_NAME),
        r#"{"schema_version":1,"runtimes":[],"extra":1}"#,
    )
    .expect("写清单");
    let report = load_registry_with_expected(&dir, &ExpectedRegistry::default());
    assert_eq!(report.rejected.len(), 1);
    assert!(report.rejected[0].1.starts_with("schema_invalid"));
    std::fs::write(
        dir.join(REGISTRY_FILE_NAME),
        r#"{"schema_version":2,"runtimes":[]}"#,
    )
    .expect("写清单");
    let report = load_registry_with_expected(&dir, &ExpectedRegistry::default());
    assert!(report.rejected[0].1.starts_with("schema_invalid"));
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn digest_mismatch_covers_missing_expectation_program_tamper_and_manifest_tamper() {
    let dir = temp_dir("digest");
    std::fs::write(dir.join("adapter.exe"), b"pristine-bytes").expect("程序");
    let document = write_manifest(&dir, &[entry("claude-code", "adapter.exe", json!([]))]);

    // 期望摘要缺失（本构建未内嵌 runtime-bundle）→ 清单一律拒绝（fail-closed）。
    let report = load_registry_with_expected(&dir, &ExpectedRegistry::default());
    assert!(report.specs.is_empty());
    assert!(report.rejected[0].1.starts_with("digest_mismatch"));
    assert!(!report.expectation_present);

    // 清单规范化摘要不匹配（字段篡改：name）→ 全部条目拒绝。
    let tampered = json!({
        "schema_version": 1,
        "runtimes": [entry("claude-code", "adapter.exe", json!([]))],
    });
    let mut tampered = tampered;
    tampered["runtimes"][0]["name"] = json!("被篡改");
    std::fs::write(
        dir.join(REGISTRY_FILE_NAME),
        serde_json::to_string_pretty(&tampered).expect("序列化"),
    )
    .expect("写清单");
    let expected = ExpectedRegistry::compute_from_dir(&dir).expect("期望（篡改后）");
    // 以「原始文档」的期望加载「被篡改的文档」→ 摘要不匹配。
    let expected_original = ExpectedRegistry::from_parts(
        true,
        Some(manifest_digest(&document)),
        vec![],
    );
    let report = load_registry_with_expected(&dir, &expected_original);
    assert!(report.specs.is_empty());
    assert!(report.rejected[0].1.starts_with("digest_mismatch"));

    // args 篡改 → 规范化摘要不匹配（同上口径；此处用同目录期望 → 通过）。
    let report = load_registry_with_expected(&dir, &expected);
    assert_eq!(report.specs.len(), 1, "同源期望必须通过");

    // program 篡改（同长度换字节）→ program sha256 不匹配。
    std::fs::write(dir.join("adapter.exe"), b"tamper-bytes!!").expect("篡改");
    let report = load_registry_with_expected(&dir, &expected);
    assert!(report.specs.is_empty());
    assert!(
        report.rejected[0].1.starts_with("digest_mismatch"),
        "{}",
        report.rejected[0].1
    );

    // program 缺失 → missing_file。
    std::fs::remove_file(dir.join("adapter.exe")).expect("删除程序");
    let report = load_registry_with_expected(&dir, &expected);
    assert!(report.rejected[0].1.starts_with("missing_file"));
}

#[test]
fn path_samples_with_spaces_non_ascii_and_portable_layout_are_not_false_rejected() {
    // 便携形态 + 用户级安装（临时目录）+ 路径含空格/非 ASCII：全部经同一逻辑锚解析。
    let base = std::env::temp_dir().join(format!("aether m7-01 便携安装 {}", std::process::id()));
    let dir = base.join("runtime-bundle");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&dir).expect("便携目录");
    std::fs::write(dir.join("aether claude 适配器.exe"), b"stub").expect("程序桩");
    let document = write_manifest(
        &dir,
        &[entry("claude-code", "aether claude 适配器.exe", json!(["--x"]))],
    );
    let expected = ExpectedRegistry::from_parts(
        true,
        Some(manifest_digest(&document)),
        ExpectedRegistry::compute_from_dir(&dir)
            .expect("期望")
            .program_digests,
    );
    let report = load_registry_with_expected(&dir, &expected);
    assert_eq!(report.specs.len(), 1, "含空格/非 ASCII/便携路径不得误杀");
    assert!(report.rejected.is_empty());
    assert_eq!(report.specs[0].manifest.args, vec!["--x".to_owned()]);
    let _ = std::fs::remove_dir_all(&base);
}

/// 编译期信任锚 ↔ 真实运行时包（M4-05 产物）一致性：由 verify 脚本构建包后运行
/// （`--ignored`；`AETHER_M7_01_BUNDLE_DIR` 与 build.rs 的
/// `AETHER_RUNTIME_BUNDLE_DIR` 必须指向同一目录）。
#[test]
#[ignore = "由 scripts/test/m7-01/verify-m7-01.mjs 构建运行时包后执行（编译期期望同源）"]
fn compiled_expectation_matches_real_bundle() {
    let dir = std::env::var("AETHER_M7_01_BUNDLE_DIR")
        .expect("必须设置 AETHER_M7_01_BUNDLE_DIR（verify 脚本口径，禁止静默跳过）");
    let report = load_registry(Path::new(&dir));
    assert!(report.manifest_present, "真实包必须含 runtimes.json：{dir}");
    assert!(
        report.expectation_present,
        "本测试二进制必须内嵌同一目录的期望摘要（AETHER_RUNTIME_BUNDLE_DIR）"
    );
    assert!(
        report.rejected.is_empty(),
        "P0 运行时包在 P1 校验下不得被误杀：{:?}",
        report.rejected
    );
    let ids: Vec<&str> = report
        .specs
        .iter()
        .map(|spec| spec.manifest.id.as_str())
        .collect();
    assert!(ids.contains(&"mock"), "测试构建包应含 mock（AETHER_BUNDLE_MOCK=1）：{ids:?}");
    assert!(ids.contains(&"claude-code"), "{ids:?}");
    assert!(ids.contains(&"codex"), "{ids:?}");
    assert!(ids.contains(&"deepseek-harness"), "{ids:?}");
    for spec in &report.specs {
        assert!(spec.manifest.official, "清单条目必须为 official");
        assert!(
            spec.manifest.program.is_absolute(),
            "canonicalize 后必须为绝对路径形态"
        );
    }
}
