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

pub mod api;
pub mod bootstrap;
pub mod cli;
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
aetherd — Aether 核心守护进程（JSON-RPC 2.0 over JSONL stdio；M10-03 HTTP/WS/CLI）

用法：
  aetherd --stdio            以 stdin/stdout 的 JSONL 传输运行核心（Electron main 使用）
  aetherd --stdio --http     在 stdio 之外并行开启本地 HTTP/WS API（默认关闭）
  aetherd --stdio --no-http  显式关闭 HTTP（覆盖 AETHERD_HTTP=1）
  aetherd --http-only        仅运行 HTTP/WS API（headless/CLI/外部消费者）
  aetherd --cli <subcommand> 以 headless CLI 调用同一业务核心
  aetherd --version          打印版本并退出
  aetherd --help             打印本帮助并退出

HTTP/WS 环境变量：
  AETHERD_HTTP=0|1               HTTP 开关（--stdio 默认 0；--http-only 强制 1）
  AETHERD_HTTP_REQUIRED=1        listener 失败致命（--stdio --http 时）
  AETHERD_HTTP_BIND=127.0.0.1    仅允许 loopback（::1 需显式；远程绑定需另立 ADR）
  AETHERD_HTTP_PORT=0            0 = 随机空闲端口；固定端口占用明确报错
  AETHERD_HTTP_CORS_ORIGIN       仅测试用单一来源（生产默认拒绝 CORS）
  AETHERD_API_TOKEN              CLI/CI token（优先于发现文件）

环境变量：
  AETHER_DATA_DIR                数据目录覆盖（必须绝对路径；仍受 A4 同步盘检测约束）
  AETHER_DATA_LOCATION_FILE      数据目录指针文件覆盖（测试/E2E 隔离）
  AETHER_RUNTIME_BUNDLE_DIR      运行时注册清单目录覆盖（默认：可执行文件同目录/runtime-bundle）

退出码：
  0 正常   10 参数/协议初始化失败   20 数据目录锁被占用（data_dir_locked）
  21 A4 同步盘/数据目录不可用（sync_dir_blocked）
  22 DB 无法打开 / 已应用迁移 checksum 不符   23 迁移执行失败（保留，M9 不触发）
  24 内部错误   25 HTTP listener 失败（--http-only 或 AETHERD_HTTP_REQUIRED=1）
";

/// 命令行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// 运行 stdio 服务（可选并行 HTTP）。
    Stdio { http: api::HttpMode },
    /// 仅运行 HTTP/WS API（headless）。
    HttpOnly,
    /// headless CLI（后续参数原样交给 `cli::run`）。
    Cli(Vec<String>),
    /// 打印版本后退出。
    Version,
    /// 打印帮助后退出。
    Help,
}

/// 解析命令行（跳过 argv[0]）。
///
/// 模式：`--stdio` / `--http-only` / `--cli` / `--version` / `--help`；`--stdio` 可附加
/// `--http` / `--no-http`（或由 `AETHERD_HTTP` 决定）。其余参数 → `Err`（退出码 10）。
pub fn parse_args<I, S>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut stdio = false;
    let mut http_only = false;
    let mut http = api::HttpMode::Default;
    let mut version = false;
    let mut help = false;
    let mut cli_args: Option<Vec<String>> = None;
    let mut args = args.into_iter().skip(1);

    while let Some(arg) = args.next() {
        let text = arg.as_ref().to_string_lossy().into_owned();
        match text.as_str() {
            "--stdio" => stdio = true,
            "--http" => http = api::HttpMode::Enabled,
            "--no-http" => http = api::HttpMode::Disabled,
            "--http-only" => http_only = true,
            "--version" | "-V" => version = true,
            "--help" | "-h" => help = true,
            "--cli" => {
                let rest: Vec<String> = args
                    .by_ref()
                    .map(|value| value.as_ref().to_string_lossy().into_owned())
                    .collect();
                cli_args = Some(rest);
                break;
            }
            _ => return Err(format!("未知参数：{text}")),
        }
    }

    let modes = [stdio, http_only, version, help, cli_args.is_some()]
        .into_iter()
        .filter(|value| *value)
        .count();
    if modes > 1 {
        return Err(
            "模式参数冲突：--stdio / --http-only / --cli / --version / --help 不能同时使用"
                .to_owned(),
        );
    }
    if http_only && http == api::HttpMode::Disabled {
        return Err("--http-only 不能与 --no-http 同时使用".to_owned());
    }
    if !stdio && http != api::HttpMode::Default {
        return Err(
            "--http/--no-http 只能与 --stdio 一起使用（headless 用 --http-only）".to_owned(),
        );
    }
    if let Some(rest) = cli_args {
        return Ok(Cli::Cli(rest));
    }
    if version {
        return Ok(Cli::Version);
    }
    if help {
        return Ok(Cli::Help);
    }
    if http_only {
        return Ok(Cli::HttpOnly);
    }
    if stdio {
        return Ok(Cli::Stdio { http });
    }
    Err("缺少运行模式：请使用 --stdio / --http-only / --cli（或 --version/--help）".to_owned())
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
        Ok(Cli::Cli(args)) => cli::run(&args),
        Ok(Cli::Stdio { http }) => match bootstrap::bootstrap() {
            Ok(context) => serve::run_stdio(context, http),
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
        Ok(Cli::HttpOnly) => match bootstrap::bootstrap() {
            Ok(context) => serve::run_http_only(context),
            Err((error, log_path)) => {
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
        assert_eq!(
            parse_args(["aetherd", "--stdio"]),
            Ok(Cli::Stdio {
                http: api::HttpMode::Default
            })
        );
        assert_eq!(
            parse_args(["aetherd", "--stdio", "--http"]),
            Ok(Cli::Stdio {
                http: api::HttpMode::Enabled
            })
        );
        assert_eq!(
            parse_args(["aetherd", "--http", "--stdio"]),
            Ok(Cli::Stdio {
                http: api::HttpMode::Enabled
            })
        );
        assert_eq!(parse_args(["aetherd", "--http-only"]), Ok(Cli::HttpOnly));
        assert_eq!(
            parse_args(["aetherd", "--cli", "health"]),
            Ok(Cli::Cli(vec!["health".to_owned()]))
        );
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
        assert!(parse_args(["aetherd", "--http-only", "--no-http"]).is_err());
        assert!(parse_args(["aetherd", "--http"]).is_err());
        assert_eq!(
            parse_args(["aetherd", "--cli", "health", "--stdio"]),
            Ok(Cli::Cli(vec!["health".to_owned(), "--stdio".to_owned()]))
        );
    }

    #[test]
    fn help_lists_exit_codes_and_env_hooks() {
        for needle in [
            "--stdio",
            "--http-only",
            "--no-http",
            "--cli",
            "AETHER_DATA_DIR",
            "AETHER_RUNTIME_BUNDLE_DIR",
            "AETHERD_HTTP_BIND",
            "AETHERD_API_TOKEN",
            "20",
            "21",
            "22",
            "24",
            "25",
        ] {
            assert!(HELP.contains(needle), "帮助缺少 {needle}");
        }
    }
}
