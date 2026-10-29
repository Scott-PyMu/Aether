/**
 * S-12 壳级致命页渲染（M9-07；UI-UX v0.17 §1.2a）。
 *
 * 纯静态最小 renderer：只通过 `window.aetherFatal`（专用 preload）读取状态与
 * 触发重试/退出/离线导出；不加载主 UI、不依赖 daemon。
 */
const REASON_LABELS = {
  daemon_start_failed: "核心进程启动失败",
  handshake_failed: "核心握手失败（超时或版本不一致）",
  a4_blocked: "数据目录检测未通过（同步盘/不可用）",
  db_open_failed: "数据库无法打开（缺失/权限/加密头损坏）",
  schema_checksum_mismatch: "已应用迁移校验失败（数据库结构可能被改动）",
  data_dir_locked: "另一个 Aether 实例正在使用数据目录",
};

const root = document.querySelector('[data-testid="shell-fatal"]');
const message = document.querySelector('[data-testid="shell-fatal-message"]');
const reason = document.querySelector('[data-testid="shell-fatal-reason"]');
const logPath = document.querySelector('[data-testid="shell-fatal-log-path"]');
const dataDir = document.querySelector('[data-testid="shell-fatal-data-dir"]');
const manualText = document.querySelector('[data-testid="shell-fatal-manual-move-text"]');
const retry = document.querySelector('[data-testid="shell-fatal-retry"]');
const exit = document.querySelector('[data-testid="shell-fatal-exit"]');
const exportButton = document.querySelector('[data-testid="shell-fatal-export-diagnostics"]');
const exportResult = document.querySelector('[data-testid="shell-fatal-export-result"]');

function setText(element, value) {
  if (element) {
    element.textContent = typeof value === "string" ? value : "";
  }
}

function setResult(text, result) {
  if (exportResult) {
    exportResult.textContent = text;
    exportResult.dataset.result = result;
  }
}

async function loadState() {
  const api = window.aetherFatal;
  if (!api) {
    setResult("壳级页面 preload 未加载", "failed");
    return;
  }
  try {
    const state = await api.getState();
    if (root) {
      root.dataset.reason = state.reason;
    }
    setText(reason, REASON_LABELS[state.reason] ?? state.reason);
    setText(message, state.message);
    setText(logPath, state.logPath);
    setText(dataDir, state.dataDir);
    setText(manualText, state.manualMove);
  } catch (error) {
    setResult(`读取失败状态失败：${error instanceof Error ? error.message : String(error)}`, "failed");
  }
}

if (retry) {
  retry.addEventListener("click", async () => {
    const api = window.aetherFatal;
    if (!api) {
      return;
    }
    retry.disabled = true;
    setResult("正在重试启动…", "pending");
    try {
      const response = await api.retry();
      if (!response.ok) {
        retry.disabled = false;
        setResult("重试失败：核心仍不可用", "failed");
      }
    } catch (error) {
      retry.disabled = false;
      setResult(`重试失败：${error instanceof Error ? error.message : String(error)}`, "failed");
    }
  });
}

if (exit) {
  exit.addEventListener("click", async () => {
    const api = window.aetherFatal;
    if (!api) {
      return;
    }
    exit.disabled = true;
    try {
      await api.exit();
    } catch {
      exit.disabled = false;
    }
  });
}

if (exportButton) {
  exportButton.addEventListener("click", async () => {
    const api = window.aetherFatal;
    if (!api) {
      return;
    }
    exportButton.disabled = true;
    setResult("正在导出诊断…", "pending");
    try {
      const response = await api.exportDiagnostics();
      if (response.ok) {
        setResult(`诊断已导出：${response.path ?? ""}`, "ok");
      } else if (response.error === "cancelled") {
        setResult("已取消导出", "cancelled");
      } else {
        setResult(`导出失败：${response.error ?? "未知错误"}`, "failed");
      }
    } catch (error) {
      setResult(`导出失败：${error instanceof Error ? error.message : String(error)}`, "failed");
    } finally {
      exportButton.disabled = false;
    }
  });
}

void loadState();
