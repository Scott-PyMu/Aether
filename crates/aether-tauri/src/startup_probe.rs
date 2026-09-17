//! M1-06 E2E 探针（仅 debug 构建；`AETHER_E2E_STARTUP_PROBE=1` 启用）。
//!
//! 驱动真实 WebView 完成 T13/T12 断言：阻塞态 DOM（仅迁移/退出、主界面不可达）、
//! 命令层 `startup_blocked`、点击「迁移到本地目录」、迁移后主界面可达。
//! 机器可读输出（stdout，一行一条，供 `scripts/test/m1-06/e2e-startup-guard.mjs`）：
//! - `AETHER_M1_06_PHASE <json>`（启动门快照）
//! - `AETHER_M1_06_REPORT <json>`（页面回报：blocked / clicked / ready / error）
//! - `AETHER_M1_06_FOCUS`（首实例收到第二实例转发并聚焦）
//! - `AETHER_M1_06_EXIT {"reason":...}`（探针终止）

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;
use crate::startup::StartupGate;

pub const PROBE_ENV: &str = "AETHER_E2E_STARTUP_PROBE";
pub const TARGET_ENV: &str = "AETHER_E2E_MIGRATE_TARGET";
pub const TRIGGER_ENV: &str = "AETHER_E2E_TRIGGER_FILE";
pub const PHASE_LINE: &str = "AETHER_M1_06_PHASE";
pub const REPORT_LINE: &str = "AETHER_M1_06_REPORT";
pub const FOCUS_LINE: &str = "AETHER_M1_06_FOCUS";
pub const EXIT_LINE: &str = "AETHER_M1_06_EXIT";

// CI 慢机冷启动可能 >90s（Defender 扫描/VM 负载）；给足窗口，避免探针提前退出。
const PROBE_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

static TERMINAL: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

/// 启动时输出启动门快照（供 E2E 断言拒绝启动阶段）。
pub fn record_phase(gate: &StartupGate) {
    if !is_enabled() {
        return;
    }
    match gate.snapshot_json() {
        Ok(snapshot) => println!("{PHASE_LINE} {snapshot}"),
        Err(error) => eprintln!("[aether] M1-06 探针快照输出失败：{error}"),
    }
}

/// 首实例聚焦回调（单实例插件触发）。
pub fn record_focus() {
    if !is_enabled() {
        return;
    }
    println!("{FOCUS_LINE}");
}

/// 监视主窗口并驱动页面状态机；终止条件由 [`e2e_startup_report`] 设置。
pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !is_enabled() {
        return;
    }
    let trigger = std::env::var_os(TRIGGER_ENV).map(PathBuf::from);
    let target = std::env::var(TARGET_ENV).unwrap_or_default();
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
            let mode = if trigger.as_ref().is_some_and(|path| path.exists()) {
                "migrate"
            } else {
                "observe"
            };
            if let Some(window) = app.get_webview_window(crate::single_instance::MAIN_WINDOW_LABEL)
            {
                let _ = window.eval(script(mode, &target));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// 页面回报入口（debug handler 注册）；`ready` / `error` 视为终态。
#[tauri::command]
pub fn e2e_startup_report(payload: Value) -> Result<(), IpcError> {
    if !is_enabled() {
        return Err(IpcError::new(
            IpcErrorCode::NotImplemented,
            "M1-06 E2E 探针未启用（仅 debug 构建 + AETHER_E2E_STARTUP_PROBE=1）",
        ));
    }
    record_report(&payload);
    let stage = payload
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stage == "ready" || stage == "error" {
        TERMINAL.store(true, Ordering::SeqCst);
    }
    Ok(())
}

fn record_report(payload: &Value) {
    match serde_json::to_string(payload) {
        Ok(serialized) => println!("{REPORT_LINE} {serialized}"),
        Err(error) => eprintln!("[aether] M1-06 探针回报序列化失败：{error}"),
    }
}

const SCRIPT_TEMPLATE: &str = r#"
(function () {
  if (!window.__aetherM106) { window.__aetherM106 = { step: 0 }; }
  var S = window.__aetherM106;
  if (!window.__aetherM106Report) {
    window.__aetherM106Report = function (payload) {
      try { window.__TAURI_INTERNALS__.invoke('e2e_startup_report', { payload: payload }); } catch (error) { }
    };
  }
  var report = window.__aetherM106Report;
  var MODE = __M106_MODE_JSON__;
  var TARGET = __M106_TARGET_JSON__;
  var gate = document.querySelector('[data-testid="startup-gate"]');
  var main = document.querySelector('[data-testid="app-version"]');
  if (S.step === 0) {
    if (main) { S.step = 3; report({ stage: 'ready', main: true, gate: false }); return; }
    if (gate) {
      S.step = 1;
      var buttons = Array.prototype.map.call(document.querySelectorAll('button'), function (b) {
        return b.getAttribute('data-testid') || b.textContent;
      });
      Promise.resolve(window.__TAURI_INTERNALS__.invoke('session_list', { payload: { limit: 50 } }))
        .then(function () {
          report({ stage: 'blocked', gate: true, main: false, buttons: buttons, blocked: 'unexpected-success' });
        })
        .catch(function (error) {
          var code = (error && error.code) ? error.code : String(error);
          report({ stage: 'blocked', gate: true, main: false, buttons: buttons, blocked: code });
        });
      return;
    }
  }
  if (S.step === 1 && MODE === 'migrate') {
    var input = document.querySelector('[data-testid="startup-target"]');
    var migrateButton = document.querySelector('[data-testid="startup-migrate"]');
    if (input && migrateButton) {
      S.step = 2;
      var setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      setter.call(input, TARGET);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      setTimeout(function () { migrateButton.click(); }, 80);
      report({ stage: 'clicked', target: TARGET });
    }
    return;
  }
  if (S.step === 2) {
    if (main) { S.step = 3; report({ stage: 'ready', main: true, gate: false }); return; }
    var error = document.querySelector('[data-testid="startup-error"]');
    if (error) { S.step = 3; report({ stage: 'error', detail: error.textContent }); }
  }
})()
"#;

fn script(mode: &str, target: &str) -> String {
    SCRIPT_TEMPLATE
        .replace("__M106_MODE_JSON__", &json_string(mode))
        .replace("__M106_TARGET_JSON__", &json_string(target))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
