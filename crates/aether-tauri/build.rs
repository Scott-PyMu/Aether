//! 构建脚本（M1-08）。
//!
//! 除 tauri-build 的常规职责外，为测试目标补一个 Common-Controls v6 清单资源：
//! tauri 默认启用 `common-controls-v6`，`windows` crate 会对
//! `comctl32!TaskDialogIndirect` 生成静态导入，但 tauri-build 仅对 bin 目标输出
//! 资源（`cargo:rustc-link-arg-bins`）。缺少清单时，测试二进制会在加载阶段以
//! `STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139)` 失败而无法运行（gnu 与 msvc 均如此）。
//! 本函数只向 `tests` 目标注入该资源，生产二进制仍由 tauri-build 生成，互不影响。

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    tauri_build::build();
    embed_common_controls_manifest_for_tests();
}

/// 与 tauri-build 生成的清单一致（`Microsoft.Windows.Common-Controls` v6）。
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
        println!("cargo:warning=写入测试清单 .rc 失败，Windows 测试二进制可能无法加载");
        return;
    }

    // embed-resource 会为 gnu 选择 windres、为 msvc 定位 RC.EXE，并输出
    // `cargo:rustc-link-arg-tests=<资源库>`（仅测试目标，不影响生产二进制）。
    let result = embed_resource::compile_for_tests(&rc_path, embed_resource::NONE);
    match result {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => {
            println!(
                "cargo:warning=测试清单资源未编译（{other}），\
                 Windows 测试二进制可能因 Common-Controls v6 缺失而无法运行"
            );
        }
    }
}
