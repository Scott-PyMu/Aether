//! M2-07 E2E 探针（仅 debug 构建；`AETHER_E2E_HEALTH_PROBE=1` 启用）。
//!
//! 驱动真实 WebView 验证 UI 健康轮询（D2/ADR-007 附录 A.3）：
//! - `observe`（默认）：等待并回报 `health-normal` / `storage-degraded` 两态；
//! - `stall`：宿主侧挂起 `health` 命令（`AETHER_E2E_HEALTH_STALL_MS`，异步等待不阻塞
//!   主线程），等待并回报 15s 无响应后的 `core-unresponsive` + `core-restart`（重启入口）。
//!
//! 机器可读输出（stdout，一行一条，供 `scripts/test/m2-07/e2e-health-monitor.mjs`）：
//! - `AETHER_M2_07_REPORT <json>`（页面回报：normal / degraded / unresponsive）
//! - `AETHER_M2_07_EXIT {"reason":...}`（探针终止）
//!
//! 探针只读 DOM（`data-testid`）并（仅 stall 模式）注入宿主侧挂起；不改生产 UI 代码路径。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;

pub const PROBE_ENV: &str = "AETHER_E2E_HEALTH_PROBE";
pub const MODE_ENV: &str = "AETHER_E2E_HEALTH_MODE";
/// stall 模式：`health` 命令返回前挂起毫秒数（默认 600000 = 观察期内不返回）。
///
/// 说明：`window.__TAURI_INTERNALS__.invoke` 由 Tauri 以不可写属性定义
/// （`Object.defineProperty` 无 `writable:true`），前端无法拦截；因此挂起注入
/// 放在宿主侧（仅 debug 构建 + 探针启用时生效），由 async 命令在运行时上等待，
/// 不阻塞主线程（`window.eval` 与 UI 定时器保持可用）。
pub const STALL_ENV: &str = "AETHER_E2E_HEALTH_STALL_MS";
/// stall 默认挂起时长（远超 15s 判定窗；进程随探针终止退出）。
pub const STALL_DEFAULT_MS: u64 = 600_000;
pub const REPORT_LINE: &str = "AETHER_M2_07_REPORT";
pub const EXIT_LINE: &str = "AETHER_M2_07_EXIT";

/// 探针总超时（stall 模式需 ≥15s 观察窗 + 启动余量；CI 冷启动给足窗口）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

static TERMINAL: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

fn mode() -> String {
    std::env::var(MODE_ENV).unwrap_or_else(|_| "observe".to_owned())
}

/// `health` 命令挂起注入（stall 模式；仅 debug 构建 + 探针启用时生效）。
///
/// 返回前等待 `AETHER_E2E_HEALTH_STALL_MS`（缺省 [`STALL_DEFAULT_MS`]）；非 stall
/// 模式立即返回。E2E 断言：15s 后 UI 显示「核心未响应」+ 重启入口。
pub async fn maybe_stall_health() {
    if !is_enabled() || mode() != "stall" {
        return;
    }
    let stall_ms = std::env::var(STALL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(STALL_DEFAULT_MS);
    tokio::time::sleep(Duration::from_millis(stall_ms)).await;
}

/// 监视主窗口并驱动页面状态机；终止条件由 [`e2e_health_report`] 设置。
pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !is_enabled() {
        return;
    }
    let mode = mode();
    std::thread::spawn(move || {
        let started = Instant::now();
        loop {
            if TERMINAL.load(Ordering::SeqCst) {
                println!("{EXIT_LINE} {{\"reason\":\"terminal\"}}");
                app.exit(0);
                return;
            }
            if started.elapsed() > PROBE_TIMEOUT {
                println!("{EXIT_LINE} {{\"reason\":\"timeout\"}}");
                app.exit(3);
                return;
            }
            if let Some(window) = app.get_webview_window(crate::single_instance::MAIN_WINDOW_LABEL)
            {
                let _ = window.eval(script(&mode));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// 页面回报入口（debug handler 注册）；`normal` / `degraded` / `unresponsive` 视为终态。
#[tauri::command]
pub fn e2e_health_report(payload: Value) -> Result<(), IpcError> {
    if !is_enabled() {
        return Err(IpcError::new(
            IpcErrorCode::NotImplemented,
            "M2-07 E2E 探针未启用（仅 debug 构建 + AETHER_E2E_HEALTH_PROBE=1）",
        ));
    }
    match serde_json::to_string(&payload) {
        Ok(serialized) => println!("{REPORT_LINE} {serialized}"),
        Err(error) => eprintln!("[aether] M2-07 探针回报序列化失败：{error}"),
    }
    let stage = payload
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(stage, "normal" | "degraded" | "unresponsive") {
        TERMINAL.store(true, Ordering::SeqCst);
    }
    Ok(())
}

const SCRIPT_TEMPLATE: &str = r#"
(function () {
  if (!window.__aetherM207) { window.__aetherM207 = { reported: false }; }
  var S = window.__aetherM207;
  if (!window.__aetherM207Report) {
    window.__aetherM207Report = function (payload) {
      try { window.__TAURI_INTERNALS__.invoke('e2e_health_report', { payload: payload }); } catch (error) { }
    };
  }
  var report = window.__aetherM207Report;
  var MODE = __M207_MODE_JSON__;

  if (MODE === 'stall') {
    if (S.reported) { return; }
    var unresponsive = document.querySelector('[data-testid="core-unresponsive"]');
    var restart = document.querySelector('[data-testid="core-restart"]');
    if (unresponsive && restart) {
      S.reported = true;
      report({
        stage: 'unresponsive',
        restart: true,
        text: unresponsive.textContent,
      });
    }
    return;
  }

  if (S.reported) { return; }
  var degraded = document.querySelector('[data-testid="storage-degraded"]');
  if (degraded) {
    S.reported = true;
    report({ stage: 'degraded', storage_state: 'persist_degraded', text: degraded.textContent });
    return;
  }
  var normal = document.querySelector('[data-testid="health-normal"]');
  if (normal) {
    S.reported = true;
    report({ stage: 'normal', storage_state: 'normal', text: normal.textContent });
  }
})()
"#;

fn script(mode: &str) -> String {
    SCRIPT_TEMPLATE.replace("__M207_MODE_JSON__", &json_string(mode))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
