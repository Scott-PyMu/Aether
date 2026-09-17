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
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;
use crate::picker::{DirectoryPicker, FixedDirectoryPicker};
use crate::startup::pointer::FailingPointerWriter;
use crate::startup::StartupGate;

pub const PROBE_ENV: &str = "AETHER_E2E_STARTUP_PROBE";
pub const TARGET_ENV: &str = "AETHER_E2E_MIGRATE_TARGET";
pub const TRIGGER_ENV: &str = "AETHER_E2E_TRIGGER_FILE";
/// E2E 注入的固定选择目录（迁移主路径：点击「选择目录…」返回该路径）。
pub const PICK_DIR_ENV: &str = "AETHER_E2E_PICK_DIR";
/// E2E 注入的「用户取消」选择器。
pub const PICK_CANCEL_ENV: &str = "AETHER_E2E_PICK_CANCEL";
/// 真实系统选择器冒烟模式（探针点击「选择目录…」，不注入替身）。
pub const PICKER_SMOKE_ENV: &str = "AETHER_E2E_PICKER_SMOKE";
/// 注入「指针写入失败」次数（复现「复制完成、写指针失败」窗口与幂等续跑；默认 1 次）。
pub const FAIL_POINTER_WRITE_ENV: &str = "AETHER_E2E_FAIL_POINTER_WRITE";
/// 「完成迁移」触发文件（存在时探针点击 `startup-finish-migration`）。
pub const FINISH_TRIGGER_ENV: &str = "AETHER_E2E_FINISH_TRIGGER_FILE";
pub const PHASE_LINE: &str = "AETHER_M1_06_PHASE";
pub const REPORT_LINE: &str = "AETHER_M1_06_REPORT";
pub const FOCUS_LINE: &str = "AETHER_M1_06_FOCUS";
pub const EXIT_LINE: &str = "AETHER_M1_06_EXIT";

// CI 慢机冷启动可能 >90s（Defender 扫描/VM 负载）；给足窗口，避免探针提前退出。
const PROBE_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

static TERMINAL: AtomicBool = AtomicBool::new(false);
/// 选择器冒烟：对话框已打开（此后不再抢焦点，避免主窗口把对话框顶到后台）。
static PICKER_OPENED: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

/// 注入目录选择器替身（仅探针启用时）：
/// - `AETHER_E2E_PICK_DIR=<abs>`：返回固定路径（迁移主路径 E2E）；
/// - `AETHER_E2E_PICK_CANCEL=1`：返回取消；
/// - 未设置：不注入（生产/冒烟走真实系统选择器）。
pub fn injected_picker() -> Option<Arc<dyn DirectoryPicker>> {
    if !is_enabled() {
        return None;
    }
    if let Some(dir) = std::env::var_os(PICK_DIR_ENV) {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(Arc::new(FixedDirectoryPicker::with_path(path)));
        }
        eprintln!(
            "[aether] {PICK_DIR_ENV} 必须是绝对路径，已忽略：{}",
            path.display()
        );
        return None;
    }
    if std::env::var_os(PICK_CANCEL_ENV).is_some() {
        return Some(Arc::new(FixedDirectoryPicker::with_cancel()));
    }
    None
}

/// 探针模式下按环境变量注入指针写入失败（E2E 复现续跑路径）。
pub fn maybe_override_pointer_writer(gate: StartupGate) -> StartupGate {
    if !is_enabled() {
        return gate;
    }
    let Some(raw) = std::env::var_os(FAIL_POINTER_WRITE_ENV) else {
        return gate;
    };
    let failures = raw.to_string_lossy().parse::<usize>().unwrap_or(1);
    gate.with_pointer_writer(Arc::new(FailingPointerWriter::new(failures)))
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
    let finish_trigger = std::env::var_os(FINISH_TRIGGER_ENV).map(PathBuf::from);
    let target = std::env::var(TARGET_ENV).unwrap_or_default();
    let picker_smoke = std::env::var_os(PICKER_SMOKE_ENV).is_some();
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
            let mode = if picker_smoke {
                "picker"
            } else if finish_trigger.as_ref().is_some_and(|path| path.exists()) {
                "finish"
            } else if trigger.as_ref().is_some_and(|path| path.exists()) {
                "migrate"
            } else {
                "observe"
            };
            if let Some(window) = app.get_webview_window(crate::single_instance::MAIN_WINDOW_LABEL)
            {
                if picker_smoke && !PICKER_OPENED.load(Ordering::SeqCst) {
                    // 冒烟需要原生对话框位于前台（本机桌面会话由外部自动化按键/截图）。
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
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
    if stage == "picker-open" || stage == "picked" {
        PICKER_OPENED.store(true, Ordering::SeqCst);
    }
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
  if (MODE === 'picker') {
    // 真实系统选择器冒烟：只点击「选择目录…」，由外部（键盘自动化）完成选择/取消。
    if (S.step === 1) {
      var smokePick = document.querySelector('[data-testid="startup-pick"]');
      if (smokePick) { S.step = 2; smokePick.click(); report({ stage: 'picker-open' }); }
      return;
    }
    if (S.step === 2) {
      var smokeInput = document.querySelector('[data-testid="startup-target"]');
      // 持续观察输入框：冒烟脚本可能多次尝试选择，每次变化都回报。
      if (smokeInput && smokeInput.value && smokeInput.value !== S.lastValue) {
        S.lastValue = smokeInput.value;
        report({ stage: 'picked', value: smokeInput.value });
      }
      return;
    }
    return;
  }
  if (S.step === 1 && MODE === 'migrate') {
    // 迁移主路径：不直接填输入框，改为点击「选择目录…」，由注入的 DirectoryPicker
    // 返回固定路径，验证 选择器 → UI → startup_migrate 全链路。
    var input = document.querySelector('[data-testid="startup-target"]');
    var pickButton = document.querySelector('[data-testid="startup-pick"]');
    var migrateButton = document.querySelector('[data-testid="startup-migrate"]');
    if (input && pickButton && migrateButton) {
      S.step = 2;
      pickButton.click();
      setTimeout(function () {
        report({ stage: 'picked', value: input.value, expected: TARGET });
        setTimeout(function () {
          migrateButton.click();
          report({ stage: 'clicked', target: input.value });
        }, 120);
      }, 200);
    }
    return;
  }
  if (MODE === 'finish') {
    // 幂等续跑：点击「完成迁移」（用已复制副本继续写指针）。
    var finishButton = document.querySelector('[data-testid="startup-finish-migration"]');
    if (finishButton && !S.finishClicked) {
      S.finishClicked = true;
      finishButton.click();
      report({ stage: 'finish-clicked' });
      return;
    }
    if (main && !S.readyReported) {
      S.readyReported = true;
      report({ stage: 'ready', main: true, gate: false });
      return;
    }
    var finishError = document.querySelector('[data-testid="startup-error"]');
    if (finishError) { report({ stage: 'finish-error', detail: finishError.textContent }); }
    return;
  }
  if (S.step === 2) {
    if (main) { S.step = 3; report({ stage: 'ready', main: true, gate: false }); return; }
    var error = document.querySelector('[data-testid="startup-error"]');
    if (error && !S.errorReported) {
      // 迁移失败（如注入的指针写入失败）：非终态，保留后续「完成迁移」路径。
      S.errorReported = true;
      report({ stage: 'migrate-error', detail: error.textContent });
    }
    var finishButton = document.querySelector('[data-testid="startup-finish-migration"]');
    if (finishButton && !S.finishVisibleReported) {
      // 前端刷新快照是异步的：「完成迁移」按钮出现后再回报一次。
      S.finishVisibleReported = true;
      var pendingTarget = document.querySelector('[data-testid="startup-pending-target"]');
      report({
        stage: 'finish-available',
        target: pendingTarget ? pendingTarget.textContent.trim() : '',
      });
    }
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
