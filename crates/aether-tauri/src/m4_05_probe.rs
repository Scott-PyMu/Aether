//! M4-05 E2E 探针（仅 debug 构建；`AETHER_E2E_M4_05_PROBE=1` 启用）。
//!
//! 驱动**真实 WebView 内联权限回环**（v1.18 承接显式化）：
//! 1. 运行时选择器点击注册清单加载的运行时（默认 `mock`，测试构建注册）；
//! 2. 通过真实表单创建会话（标题输入 + 提交按钮）；
//! 3. 发送 `permission-loop:<target>`（Mock 适配器发 `permission.request` 通知）；
//! 4. 等待真实 WebView 渲染审批卡（`permission-card`，原文/规范化对照），
//!    点击「允许（一次）」触发 `permission_resolve` 命令；
//! 5. 等待适配器回执（工具终态 + run 终态）经真实事件桥渲染完成；
//! 6. 回报三段证据并退出应用。
//!
//! 机器可读输出（stdout，一行一条；供 `scripts/test/m4-05/e2e-inline-permission-loop.mjs`）：
//! `AETHER_M4_05_REPORT <json>` / `AETHER_M4_05_EXIT <json>`。
//!
//! 探针只读 DOM（`data-testid`）并触发真实 UI 交互（点击/输入），不改生产 UI 代码路径。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};

use crate::ipc::error::IpcError;
use crate::ipc::IpcErrorCode;

pub const PROBE_ENV: &str = "AETHER_E2E_M4_05_PROBE";
/// 回环目标路径（测试侧创建的真实文件；工作区外触发 fs.write ask）。
pub const TARGET_ENV: &str = "AETHER_E2E_M4_05_TARGET";
/// 运行时 id（缺省 mock；测试构建注册）。
pub const RUNTIME_ENV: &str = "AETHER_E2E_M4_05_RUNTIME";
pub const REPORT_LINE: &str = "AETHER_M4_05_REPORT";
pub const EXIT_LINE: &str = "AETHER_M4_05_EXIT";

const PROBE_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn probe_timeout() -> Duration {
    std::env::var("AETHER_E2E_M4_05_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(PROBE_TIMEOUT)
}

static TERMINAL: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    std::env::var(PROBE_ENV).is_ok_and(|value| value == "1")
}

/// 监视主窗口并驱动页面状态机；终止条件由 [`e2e_m4_05_report`] 设置。
pub fn start<R: Runtime>(app: AppHandle<R>) {
    if !is_enabled() {
        return;
    }
    std::thread::spawn(move || {
        let started = Instant::now();
        let timeout = probe_timeout();
        let mut window_logged = false;
        let mut eval_error_logged = false;
        loop {
            if TERMINAL.load(Ordering::SeqCst) {
                println!("{EXIT_LINE} {{\"reason\":\"terminal\"}}");
                app.exit(0);
                return;
            }
            if started.elapsed() > timeout {
                println!("{EXIT_LINE} {{\"reason\":\"timeout\"}}");
                app.exit(3);
                return;
            }
            if let Some(window) = app.get_webview_window(crate::single_instance::MAIN_WINDOW_LABEL)
            {
                if !window_logged {
                    let url = window
                        .url()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|error| format!("<url-error: {error}>"));
                    println!(
                        "{REPORT_LINE} {{\"stage\":\"probe-window\",\"pid\":{},\"url\":\"{}\"}}",
                        std::process::id(),
                        url.replace('"', "'")
                    );
                    window_logged = true;
                }
                if let Err(error) = window.eval(script()) {
                    if !eval_error_logged {
                        println!(
                            "{REPORT_LINE} {{\"stage\":\"probe-eval-error\",\"error\":\"{}\"}}",
                            error.to_string().replace('"', "'")
                        );
                        eval_error_logged = true;
                    }
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

/// 页面回报入口（debug handler 注册）。
#[tauri::command]
pub fn e2e_m4_05_report(payload: Value) -> Result<(), IpcError> {
    if !is_enabled() {
        return Err(IpcError::new(
            IpcErrorCode::NotImplemented,
            "M4-05 E2E 探针未启用（仅 debug 构建 + AETHER_E2E_M4_05_PROBE=1）",
        ));
    }
    match serde_json::to_string(&payload) {
        Ok(serialized) => println!("{REPORT_LINE} {serialized}"),
        Err(error) => eprintln!("[aether] M4-05 探针回报序列化失败：{error}"),
    }
    if payload.get("stage").and_then(Value::as_str) == Some("done") {
        TERMINAL.store(true, Ordering::SeqCst);
    }
    Ok(())
}

const SCRIPT_TEMPLATE: &str = r#"
(function () {
  if (!window.__aetherM405Report) {
    window.__aetherM405Report = function (payload) {
      try { window.__TAURI_INTERNALS__.invoke('e2e_m4_05_report', { payload: payload }); } catch (error) { }
    };
  }
  var report = window.__aetherM405Report;
  var state = window.__aetherM405State;
  if (!state) {
    state = window.__aetherM405State = { step: 'select-runtime', pid: __PID__ };
  }
  var RUNTIME = __RUNTIME_JSON__;
  var TARGET = __TARGET_JSON__;

  // 诊断：直接探测事件监听/解除监听权限（失败原因回报；不参与通过判定）。
  if (!state.eventProbe) {
    state.eventProbe = true;
    try {
      var callbackId = window.__TAURI_INTERNALS__.transformCallback(function () {});
      window.__TAURI_INTERNALS__.invoke('plugin:event|listen', {
        event: 'aether://event',
        target: { kind: 'Any' },
        handler: callbackId,
      }).then(function (id) {
        report({ stage: 'event-listen-probe', ok: true, id: String(id) });
        // ADR-016 §6-B1：运行时解除监听覆盖（listen → unlisten 闭环）。
        try {
          if (window.__TAURI_EVENT_PLUGIN_INTERNALS__ && window.__TAURI_EVENT_PLUGIN_INTERNALS__.unregisterListener) {
            window.__TAURI_EVENT_PLUGIN_INTERNALS__.unregisterListener('aether://event', id);
          }
        } catch (error) { }
        window.__TAURI_INTERNALS__.invoke('plugin:event|unlisten', {
          event: 'aether://event',
          eventId: id,
        }).then(function () {
          report({ stage: 'event-unlisten-probe', ok: true });
        }).catch(function (error) {
          report({ stage: 'event-unlisten-probe', ok: false, error: String(error) });
        });
      }).catch(function (error) {
        report({ stage: 'event-listen-probe', ok: false, error: String(error) });
      });
    } catch (error) {
      report({ stage: 'event-listen-probe', ok: false, error: String(error) });
    }
  }

  function setValue(element, value) {
    var tag = element.tagName;
    var prototype = tag === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(prototype, 'value').set;
    setter.call(element, value);
    element.dispatchEvent(new Event('input', { bubbles: true }));
  }

  function textOf(element) {
    return element ? (element.textContent || '').trim() : null;
  }

  if (state.step === 'select-runtime') {
    var option = document.querySelector('[data-testid="runtime-option"][data-runtime-id="' + RUNTIME + '"]');
    var options = document.querySelectorAll('[data-testid="runtime-option"]');
    var available = [];
    for (var index = 0; index < options.length; index += 1) {
      available.push(options[index].getAttribute('data-runtime-id'));
    }
    if (option && option.getAttribute('data-status') === 'ready') {
      option.click();
      state.step = 'await-runtime-selected';
      report({ stage: 'runtime-selected', runtime: RUNTIME, available: available, pid: state.pid });
    }
    return;
  }

  if (state.step === 'await-runtime-selected') {
    var selected = document.querySelector('[data-testid="runtime-option"][data-runtime-id="' + RUNTIME + '"]');
    if (selected && selected.getAttribute('data-selected') === 'true') {
      var title = document.querySelector('[data-testid="session-title-input"]');
      if (title) {
        setValue(title, 'M4-05 内联回环 E2E');
        state.step = 'create-session';
      }
    }
    return;
  }

  if (state.step === 'create-session') {
    var submit = document.querySelector('[data-testid="session-create-submit"]');
    if (submit && !submit.disabled) {
      submit.click();
      state.step = 'await-session';
    }
    return;
  }

  if (state.step === 'await-session') {
    var item = document.querySelector('[data-testid="session-item"]');
    var input = document.querySelector('[data-testid="composer-input"]');
    if (item && input && !input.disabled) {
      setValue(input, 'permission-loop:' + TARGET);
      state.step = 'send';
    }
    return;
  }

  if (state.step === 'send') {
    var send = document.querySelector('[data-testid="composer-send"]');
    if (send && !send.disabled) {
      send.click();
      state.step = 'await-ask';
      report({ stage: 'sent', text: 'permission-loop:' + TARGET });
    }
    return;
  }

  if (state.step === 'await-ask') {
    // 诊断：等待审批卡期间低频率回报 DOM 与 IPC 数据（失败定位用，不参与通过判定）。
    state.askPolls = state.askPolls || 0;
    var lastPoll = state.askPolledAt || 0;
    if (Date.now() - lastPoll > 2000 && state.askPolls < 15) {
      state.askPolledAt = Date.now();
      state.askPolls += 1;
      var queueEl = document.querySelector('[data-testid="permission-queue-count"]');
      var emptyEl = document.querySelector('[data-testid="permission-empty"]');
      var bubbles = document.querySelectorAll('[data-testid="message-bubble"]').length;
      var bridgeEl = document.querySelector('[data-testid="event-bridge-status"]');
      report({
        stage: 'ask-poll',
        queue: queueEl ? queueEl.getAttribute('data-count') : null,
        empty: emptyEl ? textOf(emptyEl) : null,
        bubbles: bubbles,
        bridge: bridgeEl ? textOf(bridgeEl) : null,
      });
      try {
        window.__TAURI_INTERNALS__.invoke('permissions_pending', { payload: {} }).then(function (list) {
          report({
            stage: 'ask-poll-api',
            count: Array.isArray(list) ? list.length : -1,
            sample: JSON.stringify(list).slice(0, 240),
          });
        });
      } catch (error) { }
    }
    var card = document.querySelector('[data-testid="permission-card"]');
    if (card) {
      var raw = document.querySelector('[data-testid="permission-target-raw"]');
      var canonical = document.querySelector('[data-testid="permission-target-canonical"]');
      report({
        stage: 'ask',
        card: true,
        target_raw: raw ? textOf(raw) : null,
        target_canonical: canonical ? textOf(canonical) : null,
        action: textOf(card),
      });
      var allow = document.querySelector('[data-testid="permission-allow-once"]');
      if (allow) {
        state.step = 'await-complete';
        allow.click();
        report({ stage: 'allowed', scope: 'once' });
      }
    }
    return;
  }

  if (state.step === 'await-complete') {
    if (window.__aetherM405Done) { return; }
    var tool = document.querySelector('[data-testid="tool-call"]');
    var toolStatus = tool ? tool.getAttribute('data-tool-status') : null;
    var streaming = document.querySelector('[data-testid="streaming-indicator"]');
    var bubbleCount = document.querySelectorAll('[data-testid="message-bubble"]').length;
    if (toolStatus === 'completed' && !streaming && bubbleCount >= 2) {
      window.__aetherM405Done = true;
      report({
        stage: 'complete',
        tool_status: toolStatus,
        bubbles: bubbleCount,
        tool_call: textOf(tool),
        pid: state.pid,
      });
      report({ stage: 'done' });
    }
    return;
  }
})()
"#;

fn script() -> String {
    let runtime = std::env::var(RUNTIME_ENV).unwrap_or_else(|_| "mock".to_owned());
    let target = std::env::var(TARGET_ENV).unwrap_or_default();
    SCRIPT_TEMPLATE
        .replace("__PID__", &std::process::id().to_string())
        .replace(
            "__RUNTIME_JSON__",
            &serde_json::to_string(&runtime).unwrap_or_else(|_| "\"mock\"".to_owned()),
        )
        .replace(
            "__TARGET_JSON__",
            &serde_json::to_string(&target).unwrap_or_else(|_| "\"\"".to_owned()),
        )
}
