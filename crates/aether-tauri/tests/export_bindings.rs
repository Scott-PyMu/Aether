//! M3-01/T14：tauri-specta bindings 导出（仅显式运行，不进入常规测试批）。
//!
//! 运行：
//! `cargo test -p aether-tauri --test export_bindings -- --ignored --nocapture`
//!
//! 生成物：`packages/protocol/src/bindings.ts`（禁止手改，AGENTS §2.8）。
//! CI/本地校验：重新生成后 `git diff --exit-code`（T14，见 `scripts/ci/bindings.mjs`）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::path::Path;

/// 生成绑定并输出机器可读行（供校验脚本解析）。
#[test]
#[ignore = "生成入口：由 scripts/ci/bindings.mjs 显式运行（T14）"]
fn export_typescript_bindings() {
    let path = aether_tauri::bindings::default_output_path();
    let before = std::fs::read_to_string(&path).ok();
    aether_tauri::bindings::builder::<tauri::Wry>()
        .export(specta_typescript::Typescript::default(), &path)
        .expect("导出 bindings.ts 失败");
    let after = std::fs::read_to_string(&path).expect("重新读取 bindings.ts 失败");
    let changed = before.as_deref() != Some(after.as_str());
    println!(
        "AETHER_BINDINGS_EXPORTED path={} changed={changed} bytes={}",
        display_path(&path),
        after.len()
    );
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
