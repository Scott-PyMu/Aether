//! 壳层 CLI 冒烟断言（与 scripts/test/smoke-desktop.mjs 互补）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_tauri::{cli_exit_code, core_version, version};

#[test]
fn version_flags_are_handled() {
    assert_eq!(cli_exit_code(["--version"]), Some(0));
    assert_eq!(cli_exit_code(["-V"]), Some(0));
    assert_eq!(cli_exit_code(["--aether-diagnostics"]), Some(0));
}

#[test]
fn unknown_args_continue_to_gui() {
    assert_eq!(cli_exit_code(["--unknown"]), None);
    assert_eq!(cli_exit_code(Vec::<String>::new()), None);
}

#[test]
fn versions_share_single_source() {
    assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(core_version(), version());
}
