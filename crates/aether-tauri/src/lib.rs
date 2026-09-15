//! Aether 桌面壳（设计 D1：Tauri 2；D2：命令层保持薄，核心逻辑不在此 crate）。
//!
//! 本里程碑（M1-01）仅提供：启动壳、版本号显示所需的最小接口。
//! 安全基线与 IPC 校验框架自 M1-08 起落地。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/// 产品名（与 `tauri.conf.json` 的 productName 一致）。
pub const APP_NAME: &str = "Aether";

/// 应用版本号——单一版本来源（工作区 `Cargo.toml`，构建时注入）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 核心层版本号（诊断与冒烟测试使用）。
pub fn core_version() -> &'static str {
    aether_core::version()
}

/// 处理「打印信息后退出」类 CLI 参数（CI 冒烟与安装后自检使用）。
///
/// 返回 `Some(exit_code)` 表示参数已被处理、调用方应立即退出；
/// 返回 `None` 表示继续启动 GUI。
#[must_use]
pub fn cli_exit_code<I, S>(args: I) -> Option<i32>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for arg in args {
        match arg.as_ref() {
            "--version" | "-V" => {
                println!("{APP_NAME} {}", version());
                return Some(0);
            }
            "--aether-diagnostics" => {
                println!("{APP_NAME} {} (core {})", version(), core_version());
                return Some(0);
            }
            _ => {}
        }
    }
    None
}

/// 启动 Tauri 应用。
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default().run(tauri::generate_context!())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{cli_exit_code, core_version, version};

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
}
