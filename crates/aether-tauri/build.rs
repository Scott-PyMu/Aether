//! 构建脚本（M1-08；M7-01 增注册清单期望摘要生成）。
//!
//! 除 tauri-build 的标准生成外：
//! - 为 Windows 测试二进制注入 Common-Controls v6 清单（Tauri 要求
//!   `common-controls-v6`，`windows` crate 依赖 `comctl32!TaskDialogIndirect`
//!   等导出；不带清单时测试宿主在部分 Windows 环境加载失败）。该注入只作用于测试
//!   二进制（`cargo:rustc-link-arg-tests`），不进入生产 bin；
//! - `tauri.conf.json` 的 `bundle.resources` 声明安装产物内置运行时包目录
//!   `runtime-bundle/`；tauri-build 在编译期校验资源路径存在，因此先确保该目录存在；
//! - **M7-01/ADR-015 §2.2#2**：把运行时包目录的期望摘要表（清单规范化摘要 +
//!   每个 program 的 sha256）编译进二进制（`OUT_DIR/registry_expectation.rs`），
//!   作为注册清单逐条校验的信任锚。数据源目录 = `AETHER_RUNTIME_BUNDLE_DIR`
//!   环境变量覆盖 → 否则 `runtime-bundle/`（打包前由
//!   `scripts/ci/build-runtime-bundles.mjs` 生成；E2E 经环境变量让构建期与运行期
//!   指向同一目录）。清单缺失 → 生成 `PRESENT=false`（运行期清单一律拒绝）。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// 与运行期共用同一份规范化/摘要实现（`include!` 到本模块；其 `use serde_json::Value`
// 与 `sha2` 亦在本作用域生效，故此处不重复导入）。
include!("src/registry_identity.rs");

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").map(PathBuf::from).ok();
    if let Some(manifest_dir) = manifest_dir.as_deref() {
        ensure_runtime_bundle_dir(manifest_dir);
        embed_registry_expectation(manifest_dir);
    }
    tauri_build::build();
    embed_common_controls_manifest_for_tests();
}

/// M4-05：确保 `runtime-bundle/` 存在（资源声明校验用）。
///
/// 仅创建占位文件；`scripts/ci/build-runtime-bundles.mjs` 在打包前整体重建目录
/// （删除占位后写入三个官方适配器 + `runtimes.json` + `digests.json`）。
fn ensure_runtime_bundle_dir(manifest_dir: &Path) {
    let dir = manifest_dir.join("runtime-bundle");
    if let Err(error) = fs::create_dir_all(&dir) {
        println!("cargo:warning=创建 runtime-bundle 目录失败：{error}");
        return;
    }
    let readme = dir.join("README.txt");
    if !readme.exists() {
        let _ = fs::write(
            &readme,
            "运行时包目录（构建期由 scripts/ci/build-runtime-bundles.mjs 生成；本文件为占位）。\n",
        );
    }
}

/// M7-01/ADR-015：生成期望摘要表（`OUT_DIR/registry_expectation.rs`）。
///
/// 与运行期 `runtime_registry` 同口径：数据源目录内 `runtimes.json` 的
/// **规范化形态 sha256** + 每个 `program` 文件的 sha256。任一环节失败
/// （清单缺失/不可解析/程序缺失）→ 生成 `PRESENT=false` 并输出构建告警
/// （运行期将拒绝全部清单条目，fail-closed，不静默放行）。
fn embed_registry_expectation(manifest_dir: &Path) {
    let out_dir = match env::var("OUT_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => return,
    };
    let bundle_dir = match env::var("AETHER_RUNTIME_BUNDLE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => manifest_dir.join("runtime-bundle"),
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
                "cargo:warning=运行期注册清单缺失（{}）：本构建不内嵌期望摘要，清单条目将被拒绝（ADR-015 §2.2#2）。{error}",
                manifest_path.display()
            );
        }
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Err(error) => {
                println!(
                    "cargo:warning=运行期注册清单不可解析（{}）：本构建不内嵌期望摘要。{error}",
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
                                    "cargo:warning=运行期包程序缺失/不可读（{}）：本构建不内嵌期望摘要。{error}",
                                    program_path.display()
                                );
                                failed = true;
                                break;
                            }
                        }
                    }
                } else {
                    println!("cargo:warning=运行期注册清单无 runtimes 数组：不内嵌期望摘要。");
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

/// 为测试二进制注入 `Microsoft.Windows.Common-Controls` v6 清单。
const MANIFEST_RC: &str = r#"1 24
{
" <assembly xmlns=""urn:schemas-microsoft-com:asm.v1"" manifestVersion=""1.0""> "
" <dependency> "
" <dependentAssembly> "
" <assemblyIdentity "
" type=""win32"" "
" name=""Microsoft.Windows.Common-Controls"" "
" version=""6.0.0.0"" "
" processorArchitecture=""*"" "
" publicKeyToken=""6595b64144ccf1df"" "
" language=""*"" "
" /> "
" </dependentAssembly> "
" </dependency> "
" </assembly> "
}
"#;

fn embed_common_controls_manifest_for_tests() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = match env::var("OUT_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => return,
    };

    let rc_path = out_dir.join("aether-tests-manifest.rc");
    if fs::write(&rc_path, MANIFEST_RC).is_err() {
        println!("cargo:warning=写入测试清单 .rc 失败（跳过 Windows 测试宿主清单注入）");
        return;
    }

    // embed-resource 在 gnu 目标经 windres、msvc 目标经 RC.EXE 编译；
    // `cargo:rustc-link-arg-tests` 仅注入测试宿主，避免影响生产 bin。
    let result = embed_resource::compile_for_tests(&rc_path, embed_resource::NONE);
    match result {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => {
            println!(
                "cargo:warning=测试清单资源编译未完成（{other}）；若 Windows 测试宿主加载失败请检查 Common-Controls v6 注入"
            );
        }
    }
}
