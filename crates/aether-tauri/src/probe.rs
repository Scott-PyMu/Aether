//! M1-08 E2E 探针（仅 debug 构建）。
//!
//! 用途：在真实 WebView（非 MockRuntime）中验证 CSP 生效、外链导航拦截与
//! `withGlobalTauri:false`。由环境变量 `AETHER_E2E_CSP_PROBE=1` 启用；探针窗口
//! 加载 `csp-probe.html`（E2E 脚本注入到前端 dist 的测试夹具），页面通过 IPC
//! 回报观测结果，随后尝试一次外链导航，宿主打印机器可读行供脚本断言。
//!
//! 机器可读输出（stdout，一行一条）：
//! - `AETHER_E2E_PROBE_READY <label>`
//! - `AETHER_E2E_PROBE_REPORT <json>`
//! - `AETHER_E2E_NAV_ALLOW|NAV_BLOCK|NAV_EXTERNAL <url>`

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime, Webview};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;
use crate::nav::NavigationAction;

/// 启用探针的环境变量。
pub const PROBE_ENV: &str = "AETHER_E2E_CSP_PROBE";
/// 探针窗口标签。
pub const PROBE_WINDOW_LABEL: &str = "csp-probe";
/// 探针用于「结束测试进程」的测试保留域名（E2E 页面外链导航目标）。
pub const PROBE_EXIT_DOMAIN: &str = "aether-csp-probe.invalid";

pub const READY_LINE: &str = "AETHER_E2E_PROBE_READY";
pub const REPORT_LINE: &str = "AETHER_E2E_PROBE_REPORT";
pub const NAV_ALLOW_LINE: &str = "AETHER_E2E_NAV_ALLOW";
pub const NAV_BLOCK_LINE: &str = "AETHER_E2E_NAV_BLOCK";
pub const NAV_EXTERNAL_LINE: &str = "AETHER_E2E_NAV_EXTERNAL";

/// 主窗口 React 引导回执（`ui-bootstrapped` 事件已上报）。
///
/// CI 冷启动（WebView2 初始化可达 10s+）下，探针页排队回报与外链导航（触发退出）
/// 可能早于主窗口挂载；退出前等待该回执（有界），消除夹具时序竞态——断言不变。
static UI_BOOTSTRAPPED: AtomicBool = AtomicBool::new(false);
/// 退出前等待主窗口引导回执的上限（CI 冷启动余量；E2E 脚本总超时 45s）。
const UI_BOOTSTRAP_WAIT: Duration = Duration::from_secs(20);

/// 探针是否启用。
pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

/// 在 setup 阶段创建探针窗口（仅探针模式），并监视主窗口是否在加固 CSP 下完成引导。
pub fn setup_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    if !is_enabled() {
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(
        app,
        PROBE_WINDOW_LABEL,
        tauri::WebviewUrl::App("csp-probe.html".into()),
    )
    .title("Aether CSP Probe")
    .inner_size(900.0, 640.0)
    .build()?;
    println!("{READY_LINE} {}", window.label());
    watch_main_window_bootstrap(app.clone());
    Ok(())
}

/// 周期性向主窗口注入 UI 引导探针（幂等）：
/// 仅当 React 已挂载出 `data-testid="app-version"` 时回报，证明加固后的 CSP
/// 没有破坏应用自身资源的加载与执行。
fn watch_main_window_bootstrap<R: Runtime>(app: AppHandle<R>) {
    const SCRIPT: &str = r#"(function () {
        if (window.__aetherUiBootstrapped) { return; }
        if (document.querySelector('[data-testid="app-version"]')) {
            window.__aetherUiBootstrapped = true;
            window.__TAURI_INTERNALS__.invoke('e2e_probe_report', {
                payload: { event: 'ui-bootstrapped' },
            });
        }
    })()"#;

    std::thread::spawn(move || {
        // 轮询窗口覆盖 CI 冷启动（WebView2 初始化 + 主窗口挂载可达 10s+）；
        // 退出路径另行等待回执（见 `record_navigation`），两者共同消除竞态。
        for _ in 0..120 {
            if UI_BOOTSTRAPPED.load(Ordering::SeqCst) {
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval(SCRIPT);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// 记录导航决策（探针模式下的系统浏览器转交记录器，替代真实打开浏览器）。
pub fn record_navigation<R: Runtime>(
    webview: &Webview<R>,
    url: &url::Url,
    action: NavigationAction,
) {
    let line = match action {
        NavigationAction::Allow => NAV_ALLOW_LINE,
        NavigationAction::Block => NAV_BLOCK_LINE,
        NavigationAction::OpenExternal => NAV_EXTERNAL_LINE,
    };
    println!("{line} {url}");

    // 测试页面在回报完成后（或 IPC 兜底时）总会访问测试保留域名；收到该外链
    // 导航即结束探针进程（真实浏览器转交在探针模式下被记录器替代）。
    if action != NavigationAction::OpenExternal || url.host_str() != Some(PROBE_EXIT_DOMAIN) {
        return;
    }

    let app = webview.app_handle().clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        // 有界等待主窗口引导回执：探针窗口与主窗口并行加载，CI 冷启动下主窗口
        // 可能晚于探针页回报挂载；引导未回执则按上限退出（真实失败仍被 E2E 断言）。
        let deadline = std::time::Instant::now() + UI_BOOTSTRAP_WAIT;
        while !UI_BOOTSTRAPPED.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        app.exit(0);
    });
}

/// 记录页面回报的探针结果（`ui-bootstrapped` 置位退出等待标志）。
pub fn record_report(payload: &Value) {
    if payload.get("event").and_then(Value::as_str) == Some("ui-bootstrapped") {
        UI_BOOTSTRAPPED.store(true, Ordering::SeqCst);
    }
    match serde_json::to_string(payload) {
        Ok(serialized) => println!("{REPORT_LINE} {serialized}"),
        Err(error) => eprintln!("[aether] 探针回报序列化失败：{error}"),
    }
}

/// E2E 探针回报命令（仅 debug 构建注册；未启用探针时返回 `not_implemented`）。
#[tauri::command]
pub fn e2e_probe_report(payload: Value) -> Result<(), IpcError> {
    if !is_enabled() {
        return Err(IpcError::new(
            IpcErrorCode::NotImplemented,
            "E2E 探针未启用（仅 debug 构建 + AETHER_E2E_CSP_PROBE=1）",
        ));
    }
    record_report(&payload);
    Ok(())
}
