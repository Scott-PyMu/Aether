//! Aether 桌面壳（设计 D1：Tauri 2；D2：命令层保持薄，核心逻辑不在此 crate）。
//!
//! M1-08 起落地 Tauri 安全基线（设计 D7 / 评审 #7）：
//! - CSP 由 `tauri.conf.json` 冻结（[`config::EXPECTED_CSP`]），并由 E2E 断言真实阻断；
//! - `withGlobalTauri:false`，非本地导航经 [`nav`] 拦截并转交系统浏览器；
//! - IPC 命令面经 [`ipc`] 的统一参数校验框架（严格反序列化 + 路径 canonicalize +
//!   枚举白名单 + 长度上限），校验失败返回结构化错误、不落库、不透传下游；
//! - capabilities 最小 allowlist 签入（[`config`] 断言），devtools 仅 debug 可达。
//!
//! 测试统一位于 `tests/`（见 `Cargo.toml` 的说明）。

pub mod config;
pub mod ipc;
pub mod nav;

#[cfg(debug_assertions)]
mod probe;

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
    let backend: std::sync::Arc<dyn ipc::IpcBackend> =
        std::sync::Arc::new(ipc::backend::NotImplementedBackend);
    // 路径白名单根目录随 M1-06（数据目录）/ M3-05（诊断导出）接入；未配置即默认拒绝。
    let state = ipc::IpcState::new(backend, Vec::new());

    tauri::Builder::default()
        .plugin(nav::plugin())
        .invoke_handler(ipc::handler())
        .manage(state)
        .setup(|app| {
            #[cfg(debug_assertions)]
            {
                probe::setup_window(app.handle())?;
            }
            #[cfg(not(debug_assertions))]
            {
                let _ = app;
            }
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
