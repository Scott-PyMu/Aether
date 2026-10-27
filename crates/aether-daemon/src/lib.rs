//! # aether-daemon — `aetherd`（ADR-029 D-029-2/D-029-3）
//!
//! 职责（M9-03）：
//! - **预 `initialize` 致命启动检查**（[`bootstrap`]）：数据目录解析 → A4 检测 →
//!   OS 独占锁 `<data_dir>/.aetherd.lock` → DB 打开（不含 `quick_check`）→
//!   已应用迁移 checksum 校验（不执行迁移）；
//! - **stdio 传输**（[`serve`]）：JSON-RPC 2.0 over JSONL；stdout 只允许 JSONL，
//!   stderr 只放日志；`initialize` 首个请求；`shutdown` 干净退出；stdin EOF 优雅关闭；
//! - **请求路由**：59 条 daemon 命令 → `aether-app::dispatch`（业务/校验/错误映射全部在
//!   应用层；本 crate 不做业务判断）；4 条壳本地命令按 `method_not_found` 拒绝。
//!
//! 硬约束（AGENTS §2.1 / ADR-029 §3.3）：**不得**依赖 `aether-tauri`；不依赖
//! Electron/Node/窗口实现；核心 crate 禁 `unwrap/expect/panic`（AGENTS §2.2）。
//!
//! 退出码（ADR-029 §4.3）：`0` 正常；`10` 参数/协议初始化失败；`20` `data_dir_locked`；
//! `21` A4 同步盘（`sync_dir_blocked`）；`22` DB 无法打开 / checksum 不符；`23` 迁移执行
//! 失败（**保留**，M9 不触发）；`24` 内部错误。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod bootstrap;
pub mod lock;
pub mod serve;

use aether_app::{core_version, version};

pub use bootstrap::{
    Bootstrap, BootstrapError, EXIT_DATA_DIR_LOCKED, EXIT_DB_OPEN_FAILED, EXIT_INTERNAL,
    EXIT_MIGRATION_FAILED, EXIT_OK, EXIT_SYNC_DIR_BLOCKED, EXIT_USAGE,
};

/// 守护进程名（打包产物名；ADR-029 §5）。
pub const DAEMON_NAME: &str = "aetherd";

/// `--help` 文本（stdout；不进 JSONL 通道）。
pub const HELP: &str = "\
aetherd — Aether 核心守护进程（JSON-RPC 2.0 over JSONL stdio）

用法：
  aetherd --stdio          以 stdin/stdout 的 JSONL 传输运行核心（Electron main 使用）
  aetherd --version        打印版本并退出
  aetherd --help           打印本帮助并退出

环境变量：
  AETHER_DATA_DIR                数据目录覆盖（必须绝对路径；仍受 A4 同步盘检测约束）
  AETHER_DATA_LOCATION_FILE      数据目录指针文件覆盖（测试/E2E 隔离）
  AETHER_RUNTIME_BUNDLE_DIR      运行时注册清单目录覆盖（默认：可执行文件同目录/runtime-bundle）

退出码：
  0 正常   10 参数/协议初始化失败   20 数据目录锁被占用（data_dir_locked）
  21 A4 同步盘/数据目录不可用（sync_dir_blocked）
  22 DB 无法打开 / 已应用迁移 checksum 不符   23 迁移执行失败（保留，M9 不触发）
  24 内部错误
";

/// 命令行解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    /// 运行 stdio 服务。
    Stdio,
    /// 打印版本后退出。
    Version,
    /// 打印帮助后退出。
    Help,
}

/// 解析命令行（跳过 argv[0]）。
///
/// 只接受 `--stdio` / `--version` / `-V` / `--help` / `-h`；其余参数 → `Err`（退出码 10）。
pub fn parse_args<I, S>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cli: Option<Cli> = None;
    for arg in args.into_iter().skip(1) {
        let text = arg.as_ref().to_string_lossy().into_owned();
        let next = match text.as_str() {
            "--stdio" => Cli::Stdio,
            "--version" | "-V" => Cli::Version,
            "--help" | "-h" => Cli::Help,
            _ => return Err(format!("未知参数：{text}")),
        };
        if let Some(previous) = cli {
            let previous = match previous {
                Cli::Stdio => "--stdio",
                Cli::Version => "--version",
                Cli::Help => "--help",
            };
            return Err(format!("参数冲突：{previous} 与 {text} 不能同时使用"));
        }
        cli = Some(next);
    }
    cli.ok_or_else(|| "缺少运行模式：请使用 --stdio（或 --version/--help）".to_owned())
}

/// 进程入口（返回退出码；`main` 只做 `ExitCode` 转换）。
pub fn run<I, S>(args: I) -> u8
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    match parse_args(args) {
        Ok(Cli::Version) => {
            println!("{DAEMON_NAME} {} (core {})", version(), core_version());
            EXIT_OK
        }
        Ok(Cli::Help) => {
            print!("{HELP}");
            EXIT_OK
        }
        Ok(Cli::Stdio) => match bootstrap::bootstrap() {
            Ok(context) => serve::run_stdio(context),
            Err((error, log_path)) => {
                // 致命启动失败：唯一首帧 `id:null` error + 退出码（ADR-029 §4.2/§4.3）。
                let exit = serve::bootstrap_failure_exit(&error, &log_path);
                eprintln!(
                    "[aetherd] 启动失败（退出码 {exit}）：{}；日志 {}",
                    error.message(),
                    log_path.display()
                );
                exit
            }
        },
        Err(message) => {
            eprintln!("[aetherd] {message}");
            eprintln!("{HELP}");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_frozen_modes() {
        assert_eq!(parse_args(["aetherd", "--stdio"]), Ok(Cli::Stdio));
        assert_eq!(parse_args(["aetherd", "--version"]), Ok(Cli::Version));
        assert_eq!(parse_args(["aetherd", "-V"]), Ok(Cli::Version));
        assert_eq!(parse_args(["aetherd", "--help"]), Ok(Cli::Help));
        assert_eq!(parse_args(["aetherd", "-h"]), Ok(Cli::Help));
    }

    #[test]
    fn cli_rejects_missing_unknown_and_conflicting_modes() {
        assert!(parse_args(["aetherd"]).is_err());
        assert!(parse_args(["aetherd", "--bogus"]).is_err());
        assert!(parse_args(["aetherd", "--stdio", "--version"]).is_err());
    }

    #[test]
    fn help_lists_exit_codes_and_env_hooks() {
        for needle in [
            "--stdio",
            "AETHER_DATA_DIR",
            "AETHER_RUNTIME_BUNDLE_DIR",
            "20",
            "21",
            "22",
            "24",
        ] {
            assert!(HELP.contains(needle), "帮助缺少 {needle}");
        }
    }
}
