//! 构建脚本（M7-01/ADR-015 §2.2#2；M9-02 随 `runtime_registry` 迁入 aether-app）。
//!
//! 除运行期注册清单解析外，把「安装产物内置运行时包」的期望摘要表
//! （清单规范化 sha256 + 每个 program 的 sha256）编译进二进制
//! （`OUT_DIR/registry_expectation.rs`），作为注册清单逐条校验的信任锚。
//!
//! 数据源目录：`AETHER_RUNTIME_BUNDLE_DIR` 覆盖 → 迁移期打包目录
//! `crates/aether-tauri/runtime-bundle/`（`scripts/ci/build-runtime-bundles.mjs` 生成）。
//! 清单缺失/不可解析/程序缺失 → `PRESENT=false`（运行期拒绝全部清单条目，fail-closed）。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// 与运行期共用同一份规范化/摘要实现（`include!` 到本模块；其 `use serde_json::Value`
// 与 `sha2` 亦在本作用域生效，故此处不重复导入）。
include!("src/registry_identity.rs");

fn main() {
    if let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR").map(PathBuf::from) {
        embed_registry_expectation(&manifest_dir);
    }
}

/// 生成期望摘要表（`OUT_DIR/registry_expectation.rs`）。
fn embed_registry_expectation(manifest_dir: &Path) {
    let out_dir = match env::var("OUT_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => return,
    };
    let bundle_dir = match env::var("AETHER_RUNTIME_BUNDLE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => manifest_dir.join("..").join("aether-tauri").join("runtime-bundle"),
    };
    println!("cargo:rerun-if-env-changed=AETHER_RUNTIME_BUNDLE_DIR");
    let manifest_path = bundle_dir.join("runtimes.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let mut present = false;
    let mut computed_manifest_digest = String::new();
    let mut programs: Vec<(String, String)> = Vec::new();
    match fs::read_to_string(&manifest_path) {
        Err(error) => {
            println!(
                "cargo:warning=运行时注册清单缺失（{}）：本构建不内嵌期望摘要，清单条目将被拒绝（ADR-015 §2.2#2）。{error}",
                manifest_path.display()
            );
        }
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Err(error) => {
                println!(
                    "cargo:warning=运行时注册清单不可解析（{}）：本构建不内嵌期望摘要。{error}",
                    manifest_path.display()
                );
            }
            Ok(value) => {
                let mut failed = false;
                if let Some(entries) = value.get("runtimes").and_then(Value::as_array) {
                    for entry in entries {
                        let Some(program) = entry.get("program").and_then(Value::as_str) else {
                            failed = true;
                            break;
                        };
                        let program_path = bundle_dir.join(program);
                        println!("cargo:rerun-if-changed={}", program_path.display());
                        match program_digest(&program_path) {
                            Ok(digest) => programs.push((program.to_owned(), digest)),
                            Err(error) => {
                                println!(
                                    "cargo:warning=运行时包程序缺失/不可读（{}）：本构建不内嵌期望摘要。{error}",
                                    program_path.display()
                                );
                                failed = true;
                                break;
                            }
                        }
                    }
                } else {
                    println!("cargo:warning=运行时注册清单无 runtimes 数组：不内嵌期望摘要。");
                    failed = true;
                }
                if !failed {
                    computed_manifest_digest = manifest_digest(&value);
                    present = true;
                }
            }
        },
    }

    let mut code = String::new();
    code.push_str("// 由 build.rs 生成（M7-01/ADR-015 §2.2#2：期望摘要表编译进二进制）。\n");
    code.push_str(&format!("pub const PRESENT: bool = {present};\n"));
    if present {
        code.push_str(&format!(
            "pub const MANIFEST_DIGEST: &str = \"{computed_manifest_digest}\";\n"
        ));
    } else {
        code.push_str("pub const MANIFEST_DIGEST: &str = \"\";\n");
    }
    code.push_str("pub const PROGRAM_DIGESTS: &[(&str, &str)] = &[\n");
    for (program, digest) in &programs {
        code.push_str(&format!("    ({program:?}, {digest:?}),\n"));
    }
    code.push_str("];\n");
    if let Err(error) = fs::write(out_dir.join("registry_expectation.rs"), code) {
        println!("cargo:warning=写入注册清单期望摘要失败：{error}");
    }
}