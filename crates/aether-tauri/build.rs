//! 构建脚本（M1-08；M7-01 注册清单期望摘要已随 M9-02 迁至 `aether-app/build.rs`）。
//!
//! 保留职责：
//! - 为 Windows 测试二进制注入 Common-Controls v6 清单（Tauri 要求
//!   `common-controls-v6`，`windows` crate 依赖 `comctl32!TaskDialogIndirect` 等导出）；
//! - `tauri.conf.json` 的 `bundle.resources` 声明安装产物内置运行时包目录
//!   `runtime-bundle/`；tauri-build 在编译期校验资源路径存在，因此先确保该目录存在；
//! - tauri-build 的上下文生成。
//!
//! 运行时注册清单的期望摘要表（ADR-015 §2.2#2）由 `aether-app/build.rs` 生成，
//! 本 crate 不再重复生成（避免双份信任锚漂移）。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").map(PathBuf::from).ok();
    if let Some(manifest_dir) = manifest_dir.as_deref() {
        ensure_runtime_bundle_dir(manifest_dir);
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
