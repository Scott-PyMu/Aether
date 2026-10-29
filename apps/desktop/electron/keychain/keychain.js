/**
 * S-13 Keychain 重授权/降级页渲染（UI-UX v0.17 §1.2a；ADR-029 §5.4）。
 *
 * M9-07 交付独立最小 renderer 与锚点；**触发与 Keychain preflight 属 M9-09**
 * （`showKeychainPrompt` 由 main 暴露，M9-09 接线）。本页不删除任何旧条目，
 * 只提供重试/口令/跳过（只读/降级）三个动作，结果由 main 回执。
 */
const root = document.querySelector('[data-testid="keychain-reauth"]');
const reason = document.querySelector('[data-testid="keychain-reauth-reason"]');
const notice = document.querySelector('[data-testid="keychain-reauth-notice"]');
const retry = document.querySelector('[data-testid="keychain-reauth-retry"]');
const skip = document.querySelector('[data-testid="keychain-reauth-skip"]');
const passphrase = document.querySelector(
  '[data-testid="keychain-reauth-passphrase"] input',
);

function setText(element, value) {
  if (element) {
    element.textContent = typeof value === "string" ? value : "";
  }
}

async function loadState() {
  const api = window.aetherKeychain;
  if (!api) {
    setText(notice, "壳级页面 preload 未加载");
    return;
  }
  try {
    const state = await api.getState();
    if (root) {
      root.dataset.scope = state.scope;
      root.dataset.mode = state.mode;
    }
    setText(reason, state.reason);
    setText(notice, state.notice ?? "");
  } catch (error) {
    setText(notice, `读取状态失败：${error instanceof Error ? error.message : String(error)}`);
  }
}

if (retry) {
  retry.addEventListener("click", async () => {
    const api = window.aetherKeychain;
    if (!api) {
      return;
    }
    retry.disabled = true;
    setText(notice, "正在重试授权…");
    try {
      const response = await api.retry();
      setText(notice, response.ok ? "" : `重试失败：${response.error ?? "未知错误"}`);
    } finally {
      retry.disabled = false;
    }
  });
}

if (skip) {
  skip.addEventListener("click", async () => {
    const api = window.aetherKeychain;
    if (!api) {
      return;
    }
    skip.disabled = true;
    try {
      const response = await api.skip();
      setText(notice, response.ok ? "" : `跳过失败：${response.error ?? "未知错误"}`);
    } finally {
      skip.disabled = false;
    }
  });
}

void loadState();
