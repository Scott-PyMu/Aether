//! M3-06 E2E 探针（仅 debug 构建；`AETHER_E2E_M3_06_PROBE=1` 启用）。
//!
//! 驱动真实 WebView 验证「存储降级（只读）→ 恢复引导 → `app_restart` → 重启后启动自检」：
//! - **首次启动**（标记文件不存在）：等待 UI 渲染 `storage-degraded` 横幅，回报
//!   `{stage:"degraded", restart_button, composer_disabled, hint, hot_recovery}`；
//!   随后写标记文件并点击 `storage-degraded-restart`（命令层 `app_restart`）；
//! - **重启后**（标记文件存在，由重启进程继承同一数据目录）：再次等待
//!   `storage-degraded`（证明 D2 启动序列自检 + UI 重建完成），回报
//!   `{stage:"restarted", pid}`，随后退出应用。
//!
//! 机器可读输出（stdout，一行一条；供 `scripts/test/m3-06/e2e-degraded-recovery.mjs`）：
//! `AETHER_M3_06_REPORT <json>` / `AETHER_M3_06_EXIT <json>`。
//!
//! 探针只读 DOM（`data-testid`）并触发真实 UI 交互（点击重启按钮）；不改生产 UI 代码路径。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;

pub const PROBE_ENV: &str = "AETHER_E2E_M3_06_PROBE";
pub const REPORT_LINE: &str = "AETHER_M3_06_REPORT";
pub const EXIT_LINE: &str = "AETHER_M3_06_EXIT";

/// 重启标记文件名（写于数据目录；重启进程据此进入「重启后」分支）。
pub const RESTART_MARKER: &str = "m3-06-restart.marker";

/// 探针总超时（CI 冷启动 + 重启两轮给足窗口）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

static TERMINAL: AtomicBool = AtomicBool::new(false);
/// 降级态已回报（写标记 + 置位「可点击重启」）。
static RESTART_ARMED: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

/// 重启标记路径（数据目录内）。
pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RESTART_MARKER)
}

/// 监视主窗口并驱动页面状态机；终止条件由 [`e2e_m3_06_report`] 设置。
pub fn start<R: Runtime>(app: AppHandle<R>, data_dir: PathBuf) {
    if !is_enabled() {
        return;
    }
    let restarted = marker_path(&data_dir).is_file();
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
                let _ = window.eval(script(restarted, RESTART_ARMED.load(Ordering::SeqCst)));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// 页面回报入口（debug handler 注册）。
///
/// `stage=degraded`：写重启标记并置位「可点击重启」；`stage=restarted`：终止探针。
#[tauri::command]
pub fn e2e_m3_06_report(payload: Value) -> Result<(), IpcError> {
    if !is_enabled() {
        return Err(IpcError::new(
            IpcErrorCode::NotImplemented,
            "M3-06 E2E 探针未启用（仅 debug 构建 + AETHER_E2E_M3_06_PROBE=1）",
        ));
    }
    match serde_json::to_string(&payload) {
        Ok(serialized) => println!("{REPORT_LINE} {serialized}"),
        Err(error) => eprintln!("[aether] M3-06 探针回报序列化失败：{error}"),
    }
    match payload
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "degraded" => {
            if !RESTART_ARMED.swap(true, Ordering::SeqCst) {
                if let Some(marker) = payload.get("marker").and_then(Value::as_str) {
                    if let Err(error) = std::fs::write(marker, "restart-armed") {
                        eprintln!("[aether] M3-06 探针写重启标记失败：{error}");
                    }
                }
            }
        }
        "restarted" => TERMINAL.store(true, Ordering::SeqCst),
        _ => {}
    }
    Ok(())
}

const SCRIPT_TEMPLATE: &str = r#"
(function () {
  if (!window.__aetherM306Report) {
    window.__aetherM306Report = function (payload) {
      try { window.__TAURI_INTERNALS__.invoke('e2e_m3_06_report', { payload: payload }); } catch (error) { }
    };
  }
  var report = window.__aetherM306Report;
  var RESTARTED = __RESTARTED__;
  var ARMED = __ARMED__;
  var PID = __PID__;
  var degraded = document.querySelector('[data-testid="storage-degraded"]');

  if (RESTARTED) {
    if (window.__aetherM306Restarted) { return; }
    if (degraded) {
      window.__aetherM306Restarted = true;
      report({ stage: 'restarted', pid: PID });
    }
    return;
  }

  if (ARMED) {
    if (window.__aetherM306Clicked) { return; }
    var button = document.querySelector('[data-testid="storage-degraded-restart"]');
    if (button) {
      window.__aetherM306Clicked = true;
      button.click();
    }
    return;
  }

  if (window.__aetherM306Observed) { return; }
  if (degraded) {
    var restart = document.querySelector('[data-testid="storage-degraded-restart"]');
    var send = document.querySelector('[data-testid="composer-send"]');
    var hint = document.querySelector('[data-testid="composer-degraded-hint"]');
    var hot = document.querySelector(
      '[data-testid*="hot-recovery"], [data-testid*="auto-recover"], [data-testid*="one-click-recovery"]',
    );
    window.__aetherM306Observed = true;
    report({
      stage: 'degraded',
      pid: PID,
      marker: __MARKER_JSON__,
      restart_button: Boolean(restart),
      composer_disabled: send ? Boolean(send.disabled) : null,
      hint: hint ? hint.textContent : null,
      hot_recovery: Boolean(hot),
      text: degraded.textContent,
    });
  }
})()
"#;

fn script(restarted: bool, armed: bool) -> String {
    let marker = std::env::var("AETHER_DATA_DIR")
        .ok()
        .map(|data_dir| marker_path(Path::new(&data_dir)))
        .map(|path| serde_json::to_string(&path.to_string_lossy().to_string()).unwrap_or_default())
        .unwrap_or_else(|| "null".to_owned());
    SCRIPT_TEMPLATE
        .replace("__RESTARTED__", if restarted { "true" } else { "false" })
        .replace("__ARMED__", if armed { "true" } else { "false" })
        .replace("__PID__", &std::process::id().to_string())
        .replace("__MARKER_JSON__", &marker)
}
