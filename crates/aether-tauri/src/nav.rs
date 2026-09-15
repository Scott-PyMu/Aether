//! 导航拦截与系统浏览器转交（设计 D7 / 评审 #7）。
//!
//! 规则：仅允许本地源（Tauri 资源协议、dev 服务器）导航；非本地 `http(s)` 一律
//! 转交系统浏览器且不进入 WebView；其余 scheme（`file:`、`data:`、`javascript:` 等）
//! 直接阻断。`withGlobalTauri:false` 由配置断言 + E2E 双层覆盖。

use std::process::Command;

use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, Webview};
use url::Url;

/// 导航决策（纯分类结果，便于单测与 E2E 记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationAction {
    /// 本地源，放行。
    Allow,
    /// 非本地 http(s)，拦截并转交系统浏览器。
    OpenExternal,
    /// 其他 scheme，直接阻断。
    Block,
}

/// 分类导航目标。`dev_url` 仅在 dev 构建传入（`tauri.conf.json` 的 devUrl）。
pub fn classify(url: &Url, dev_url: Option<&Url>) -> NavigationAction {
    if is_local(url, dev_url) {
        return NavigationAction::Allow;
    }
    match url.scheme() {
        "http" | "https" => NavigationAction::OpenExternal,
        _ => NavigationAction::Block,
    }
}

/// 是否为应用自身的本地源。
pub fn is_local(url: &Url, dev_url: Option<&Url>) -> bool {
    match url.scheme() {
        "tauri" => matches!(url.host_str(), Some("localhost") | Some("tauri.localhost")),
        "http" | "https" => {
            url.host_str() == Some("tauri.localhost") || dev_url_matches(url, dev_url)
        }
        "about" => url.as_str() == "about:blank",
        _ => false,
    }
}

fn dev_url_matches(url: &Url, dev_url: Option<&Url>) -> bool {
    let Some(dev_url) = dev_url else {
        return false;
    };
    url.scheme() == dev_url.scheme()
        && url.host() == dev_url.host()
        && url.port_or_known_default() == dev_url.port_or_known_default()
}

/// 系统浏览器转交（可注入，便于单测；生产实现直接 spawn 系统命令）。
pub trait ExternalOpener: Send + Sync + 'static {
    fn open(&self, url: &Url) -> std::io::Result<()>;
}

/// 生产实现：按平台调用系统浏览器，不经 shell，URL 仅作为参数传递。
#[derive(Debug, Default)]
pub struct SystemBrowserOpener;

impl ExternalOpener for SystemBrowserOpener {
    fn open(&self, url: &Url) -> std::io::Result<()> {
        let (program, args) = browser_command(url);
        Command::new(program).args(args).spawn().map(|_| ())
    }
}

/// 按平台构造打开浏览器的命令（纯函数，单测不触发真实浏览器）。
///
/// - Windows：`rundll32 url.dll,FileProtocolHandler <url>`（不经 `cmd.exe`，避免
///   元字符注入；URL 作为独立参数传递）；
/// - macOS：`open <url>`；Linux：`xdg-open <url>`。
pub fn browser_command(url: &Url) -> (&'static str, Vec<String>) {
    #[cfg(target_os = "windows")]
    {
        (
            "rundll32.exe",
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        )
    }
    #[cfg(target_os = "macos")]
    {
        ("open", vec![url.to_string()])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        ("xdg-open", vec![url.to_string()])
    }
}

/// 执行导航决策：返回是否允许导航；外链经 `opener` 转交且不进入 WebView。
pub fn apply_navigation_decision<O: ExternalOpener>(
    action: NavigationAction,
    url: &Url,
    opener: &O,
) -> bool {
    match action {
        NavigationAction::Allow => true,
        NavigationAction::Block => false,
        NavigationAction::OpenExternal => {
            if let Err(error) = opener.open(url) {
                eprintln!("[aether] 转交系统浏览器失败：{error}（{url}）");
            }
            false
        }
    }
}

/// 导航拦截插件：对所有 webview（含配置创建的窗口）生效。
pub fn plugin<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("aether-navigation-guard")
        .on_navigation(|webview: &Webview<R>, url: &Url| handle_navigation(webview, url))
        .build()
}

fn handle_navigation<R: Runtime>(webview: &Webview<R>, url: &Url) -> bool {
    let dev_url = if tauri::is_dev() {
        webview.app_handle().config().build.dev_url.clone()
    } else {
        None
    };
    let action = classify(url, dev_url.as_ref());

    #[cfg(debug_assertions)]
    {
        if crate::probe::is_enabled() {
            crate::probe::record_navigation(webview, url, action);
            return action == NavigationAction::Allow;
        }
    }

    apply_navigation_decision(action, url, &SystemBrowserOpener)
}
