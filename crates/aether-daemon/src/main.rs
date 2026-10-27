//! `aetherd` 进程入口（ADR-029 D-029-2）。
//!
//! 逻辑全部在 `aether_daemon::run`（可单测）；本文件只做 `ExitCode` 转换。

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(aether_daemon::run(std::env::args_os()))
}
