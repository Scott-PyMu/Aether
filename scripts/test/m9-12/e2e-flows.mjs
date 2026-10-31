#!/usr/bin/env node
/**
 * M9-12 Playwright Electron E2E（真实 daemon + Mock 运行时；ADR-029 §8.2）。
 *
 * 覆盖 P2 计划 M9-12 DoD-2 的 13 条用户路径：
 *   session_create / session_send / session_interrupt /
 *   permission_allow / permission_deny / permission_timeout /
 *   backup / export / audit / pause_resume / settings / providers / presets。
 *
 * 设计口径：
 * - **真实** `aetherd`（`--features test-hooks` 构建；只压缩 300s 审批超时为可测值，
 *   不注册任何命令、不进入 manifest）+ 隔离 Mock runtime-bundle（ADR-008）；
 * - 通过真实 renderer DOM（`data-testid` 锚点）驱动；RPC 仅用于断言后端事实；
 * - 每次运行写 `evidence.json`（含 `attempts`），失败不静默重试；trace/screenshot
 *   统一落 `dist-out/m9-12-e2e/<run>/`，供 CI artifact 归档；
 * - 临时数据目录/用户目录隔离，运行结束回收 Electron/daemon 进程树。
 *
 * 前置：
 *   pnpm prepare:m9-12:e2e   # Mock bundle + aetherd --features test-hooks
 *   pnpm build:desktop
 */
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveElectronExecutable } from "../../ci/electron-bin.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const appRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(appRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");
const electronPath = resolveElectronExecutable({ repoRoot });
const daemonPath = path.join(
  repoRoot,
  "target",
  "debug",
  process.platform === "win32" ? "aetherd.exe" : "aetherd",
);
const bundleDir = path.resolve(
  repoRoot,
  process.env.AETHER_M9_12_BUNDLE ?? path.join("dist-out", "m9-12-runtime-bundle"),
);
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");
const permissionTimeoutMs = Number(process.env.AETHER_M9_12_PERMISSION_TIMEOUT_MS ?? 1_500);

/** 顺序与 DoD-2 清单一致；`fixtures/flows.json` 是 verify:m9-12 的机器可读副本。 */
const FLOWS = [
  "session_create",
  "session_send",
  "session_interrupt",
  "permission_allow",
  "permission_deny",
  "permission_timeout",
  "backup",
  "export",
  "audit",
  "pause_resume",
  "settings",
  "providers",
  "presets",
];

const startedAt = new Date();
const runId = startedAt.toISOString().replace(/[:.]/g, "-");
const artifactRoot = path.join(repoRoot, "dist-out", "m9-12-e2e", runId);
const screenshotDir = path.join(artifactRoot, "screenshots");
const tracePath = path.join(artifactRoot, "trace.zip");
const evidencePath = path.join(artifactRoot, "evidence.json");

mkdirSync(screenshotDir, { recursive: true });
rmSync(tracePath, { force: true });

const results = [];
const workspaces = [];
let app = null;
let page = null;
let sessionId = null;
let traceStarted = false;
let fatal = null;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let lastError;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

function record(flow, ok, detail = "") {
  results.push({ flow, ok, detail, at: new Date().toISOString() });
  console.log(`${ok ? "PASS" : "FAIL"}  ${flow}${detail ? `  ${detail}` : ""}`);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function killTree(pid) {
  if (!pid || pid <= 0) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
      stdio: "ignore",
      timeout: 10_000,
    });
  } else {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // 已退出。
      }
    }
  }
}

function safeRemove(dir) {
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-12-e2e] 清理临时目录失败（忽略）：${dir} ${error.message}`);
  }
}

function workspace() {
  const base = path.join(repoRoot, "dist-out", "m9-12-e2e", "work");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, "run-"));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const downloads = path.join(root, "downloads");
  const outside = path.join(root, "outside");
  const tmp = path.join(root, "tmp");
  for (const dir of [dataDir, userData, downloads, outside, tmp]) {
    mkdirSync(dir, { recursive: true });
  }
  const item = { root, dataDir, userData, downloads, outside, tmp };
  workspaces.push(item);
  return item;
}

function launchEnv(ws) {
  return {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_E2E_DOWNLOADS_DIR: ws.downloads,
    AETHER_USER_DATA_DIR: ws.userData,
    AETHER_DATA_DIR: ws.dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(ws.root, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: bundleDir,
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
    // macOS CI/无 Secret Service 环境走 A3 加密文件（ADR-020）；测试口令仅 E2E 使用。
    AETHER_SECRETS_PASSPHRASE: "m9-12-e2e-passphrase",
    AETHERD_BIN: daemonPath,
    // 仅 test-hooks 构建读取；普通/release 构建忽略（boot.rs）。
    AETHER_TEST_PERMISSION_TIMEOUT_MS: String(permissionTimeoutMs),
    USERPROFILE: ws.root,
    HOME: ws.root,
    TEMP: ws.tmp,
    TMP: ws.tmp,
  };
}

async function launch(ws) {
  if (!existsSync(daemonPath)) {
    throw new Error(`缺少 aetherd：${daemonPath}（先 pnpm prepare:m9-12:e2e）`);
  }
  if (!existsSync(path.join(bundleDir, "runtimes.json"))) {
    throw new Error(`缺少 Mock runtime-bundle：${bundleDir}（先 pnpm prepare:m9-12:e2e）`);
  }
  app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
    cwd: repoRoot,
    env: launchEnv(ws),
    timeout: 30_000,
  });
  await waitFor(
    async () => {
      const pages = app.windows().filter((item) => !item.isClosed());
      return pages.find((item) => item.url() === APP_URL) ?? null;
    },
    30_000,
    "主窗口加载",
  );
  page = app.windows().find((item) => item.url() === APP_URL) ?? (await app.firstWindow());
  await page.waitForSelector("[data-testid='workbench']", { timeout: 20_000 });
  await waitFor(
    async () => {
      const state = await app.evaluate(
        () => globalThis.__aetherShell?.snapshot?.() ?? null,
      );
      return state?.ready ? state : null;
    },
    40_000,
    "daemon initialize",
  );
  await waitFor(
    async () => {
      const envelope = await rpcRaw("health", {});
      return envelope?.ok === true ? envelope : null;
    },
    30_000,
    "health",
  );
  const runtimes = await rpcOk("runtimes_list", {});
  assert(
    Array.isArray(runtimes) && runtimes.some((runtime) => runtime.id === "mock"),
    "缺少 mock 运行时：先执行 pnpm prepare:m9-12:e2e（bundle 摘要与 aetherd 必须同源）",
  );
  // 默认右栏可能展开并覆盖输入区（视图宽度 < 1440 时）；E2E 先收起，保证点击
  // composer 不被 `right-panel` 子树拦截。
  const rightPanel = page.locator("[data-testid='right-panel']");
  if ((await rightPanel.getAttribute("data-open")) === "true") {
    await page.click("[data-testid='right-panel-toggle']");
    await page.waitForSelector("[data-testid='right-panel'][data-open='false']", {
      timeout: 5_000,
    });
  }
  try {
    const context = app.context();
    await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
    traceStarted = true;
  } catch (error) {
    console.warn(`[m9-12-e2e] trace 启动失败（继续；screenshot 仍归档）：${error.message}`);
  }
  return page;
}

async function rpcRaw(method, params) {
  return page.evaluate(
    ([name, value]) => window.aether.rpc(name, value),
    [method, params],
  );
}

async function rpcOk(method, params) {
  const envelope = await rpcRaw(method, params);
  if (!envelope || envelope.ok !== true) {
    throw new Error(`RPC ${method} 失败：${JSON.stringify(envelope?.error ?? envelope)}`);
  }
  return envelope.result;
}

async function sessions() {
  const value = await rpcOk("session_list", {});
  return Array.isArray(value) ? value : [];
}

async function sessionById(id) {
  return (await sessions()).find((item) => item.id === id) ?? null;
}

async function waitSessionStatus(id, expected, timeoutMs = 20_000) {
  return waitFor(
    async () => {
      const current = await sessionById(id);
      return current && expected.includes(current.status) ? current : null;
    },
    timeoutMs,
    `会话 ${id} 状态 ∈ ${expected.join("/")}`,
  );
}

async function waitSessionIdle(id, timeoutMs = 25_000) {
  return waitSessionStatus(id, ["idle", "failed", "paused"], timeoutMs);
}

async function messages(id) {
  return rpcOk("messages_page", { session_id: id, limit: 200 });
}

async function auditRows(filter = {}) {
  const response = await rpcOk("audit_query", { limit: 200, ...filter });
  return response?.rows ?? [];
}

async function screenshot(name) {
  if (!page) return;
  try {
    await page.screenshot({
      path: path.join(screenshotDir, `${name}.png`),
      fullPage: true,
    });
  } catch (error) {
    console.warn(`[m9-12-e2e] screenshot ${name} 失败：${error.message}`);
  }
}

async function openUserMenu(item) {
  await page.click("[data-testid='user-menu-toggle']");
  await page.waitForSelector("[data-testid='user-menu'].show", { timeout: 5_000 });
  await page.click(`[data-testid='${item}']`);
}

async function closeOverlay() {
  const back = page.locator("[data-testid='overlay-back']:visible").last();
  await back.click();
  await page.waitForSelector("[data-testid='settings-page']", { state: "detached", timeout: 5_000 }).catch(() => {});
}

async function openSettingsTab(tab) {
  await openUserMenu("settings-open");
  await page.waitForSelector("[data-testid='settings-page']", { timeout: 10_000 });
  await page.click(`[data-testid='settings-tab-${tab}']`);
}

async function createSessionByUi(title) {
  await page.click("[data-testid='new-session-open']");
  await page.waitForSelector("[data-testid='session-create-form'][data-open='true']", {
    timeout: 5_000,
  });
  await page.click("[data-testid='agent-picker-toggle']");
  await page.waitForSelector(
    "[data-testid='runtime-option'][data-runtime-id='mock']",
    { timeout: 10_000 },
  );
  await page.click("[data-testid='runtime-option'][data-runtime-id='mock']");
  await page.fill("[data-testid='session-title-input']", title);
  await page.click("[data-testid='session-create-submit']");
  await waitFor(
    async () => {
      const list = await sessions();
      return list.find((item) => item.title === title) ?? null;
    },
    15_000,
    "session_create → session_list",
  );
  return (await sessions()).find((item) => item.title === title);
}

async function sendMessage(text) {
  await page.fill("[data-testid='composer-input']", text);
  await page.click("[data-testid='composer-send']");
  await waitFor(
    async () => {
      const bubbles = page.locator("[data-testid='message-bubble'][data-role='user']");
      const count = await bubbles.count();
      for (let index = 0; index < count; index += 1) {
        const value = await bubbles.nth(index).textContent();
        if (value?.includes(text)) return true;
      }
      return false;
    },
    15_000,
    `用户气泡：${text}`,
  );
}

async function openSessionActions() {
  await page.click("[data-testid='session-actions-toggle']");
  await page.waitForSelector("[data-testid='session-actions-menu'].show", { timeout: 5_000 });
}

async function waitPermissionCard(timeoutMs = 20_000) {
  return page.waitForSelector(
    "[data-testid='permission-card'][data-status='pending']",
    { timeout: timeoutMs },
  );
}

// ===== Flows =====

const flowImpl = {
  async session_create() {
    const created = await createSessionByUi("M9-12 session");
    assert(created, "会话未出现在 session_list");
    sessionId = created.id;
    assert(created.runtime_id === "mock", `runtime_id=${created.runtime_id}`);
    await page.waitForSelector("[data-testid='session-item']", { timeout: 10_000 });
    const latency = await page.getAttribute("[data-testid='create-latency-ms']", "data-ms");
    assert(latency && Number(latency) < 2_000, `session_create 延迟 ${latency}ms`);
    return `id=${sessionId} runtime=mock createMs=${latency}`;
  },

  async session_send() {
    await sendMessage("hello m9-12");
    await waitFor(
      async () => {
        const bubbles = page.locator("[data-testid='message-bubble'][data-role='assistant']");
        return (await bubbles.count()) > 0;
      },
      20_000,
      "assistant 气泡",
    );
    await waitSessionIdle(sessionId);
    const response = await messages(sessionId);
    assert((response.messages ?? []).length >= 2, "messages_page 消息不足 2 条");
    assert(
      response.events.some((event) => event.type === "run.completed"),
      "缺少 run.completed 事件",
    );
    return `messages=${(response.messages ?? []).length} events=${response.events.length}`;
  },

  async session_interrupt() {
    await sendMessage("long");
    await waitFor(
      async () => page.locator("[data-testid='streaming-indicator']").count(),
      15_000,
      "long 流式开始",
    );
    await openSessionActions();
    await page.click("[data-testid='interrupt']");
    await waitSessionStatus(sessionId, ["idle", "failed", "paused"], 20_000);
    const response = await messages(sessionId);
    const cancelled = response.events.find(
      (event) =>
        event.type === "run.cancelled" &&
        (event.payload?.reason === "user_interrupt" ||
          event.payload?.reason === "interrupted"),
    );
    assert(cancelled, "缺少 run.cancelled(user_interrupt) 事件");
    return `run.cancelled reason=${cancelled.payload.reason}`;
  },

  async permission_allow() {
    // 策略基准 = dataDir；区外写才会 ask（D9 workspace_write 矩阵）。
    const target = path.join(workspaces[0].outside, "m9-12-allow.txt");
    await sendMessage(`permission-loop:${target}`);
    await waitPermissionCard();
    await page.waitForSelector("[data-testid='permission-timeout-note']", { timeout: 5_000 });
    await page.click("[data-testid='permission-allow-once']");
    await page.waitForSelector("[data-testid='permission-card'][data-status='pending']", {
      state: "hidden",
      timeout: 15_000,
    });
    await waitSessionIdle(sessionId);
    const rows = await auditRows({ action: "permission.resolved" });
    assert(rows.length >= 1, "缺少 permission.resolved 审计");
    const response = await messages(sessionId);
    assert(
      response.events.some((event) => event.type === "permission.resolved"),
      "缺少 permission.resolved 事件",
    );
    return `target=${target} audit=${rows.length}`;
  },

  async permission_deny() {
    const target = path.join(workspaces[0].outside, "m9-12-deny.txt");
    await sendMessage(`permission-loop:${target}`);
    await waitPermissionCard();
    await page.click("[data-testid='permission-deny']");
    await page.waitForSelector("[data-testid='permission-card'][data-status='pending']", {
      state: "hidden",
      timeout: 15_000,
    });
    await waitSessionIdle(sessionId);
    const response = await messages(sessionId);
    const failed = response.events.find(
      (event) =>
        event.type === "tool.call_failed" &&
        event.payload?.error?.code === "denied",
    );
    assert(failed, "缺少 tool.call_failed(denied)");
    return `target=${target} tool.call_failed=denied`;
  },

  async permission_timeout() {
    const target = path.join(workspaces[0].outside, "m9-12-timeout.txt");
    await sendMessage(`permission-loop:${target}`);
    await waitPermissionCard();
    const pending = await rpcOk("permissions_pending", { session_id: sessionId });
    const ticket = Array.isArray(pending)
      ? pending.find((item) => item.session_id === sessionId)
      : null;
    assert(
      ticket?.timeout_ms === permissionTimeoutMs,
      `test-hooks 审批超时=${ticket?.timeout_ms}（期望 ${permissionTimeoutMs}）`,
    );
    const note = await page.getAttribute(
      "[data-testid='permission-timeout-note']",
      "data-remaining-ms",
    );
    assert(Number(note) > 0, `timeout note=${note}`);
    await page.waitForSelector("[data-testid='permission-card'][data-status='pending']", {
      state: "hidden",
      timeout: Math.max(15_000, permissionTimeoutMs * 4),
    });
    await waitSessionIdle(sessionId);
    await waitFor(
      async () => {
        const rows = await auditRows({ action: "permission.timeout" });
        return rows.length >= 1 ? rows : null;
      },
      15_000,
      "permission.timeout 审计",
    );
    return `timeoutMs=${permissionTimeoutMs} note=${note}`;
  },

  async pause_resume() {
    await openSessionActions();
    const button = page.locator("[data-testid='session-pause-toggle']");
    await button.waitFor({ state: "visible", timeout: 5_000 });
    assert((await button.getAttribute("data-disabled")) !== "true", "暂停按钮被禁用");
    await button.click();
    await waitSessionStatus(sessionId, ["paused"], 15_000);
    await page.waitForSelector("[data-testid='paused-notice']", { timeout: 5_000 });
    await button.click();
    await waitSessionStatus(sessionId, ["idle"], 15_000);
    await page.waitForSelector("[data-testid='paused-notice']", {
      state: "detached",
      timeout: 5_000,
    });
    return "paused → idle";
  },

  async export() {
    await openSessionActions();
    await page.click("[data-testid='session-export']");
    const result = await waitFor(
      async () => {
        const node = page.locator("[data-testid='session-export-result']");
        if ((await node.count()) === 0) return null;
        const value = await node.getAttribute("data-result");
        return value === "ok" || value === "failed" ? value : null;
      },
      20_000,
      "session-export-result",
    );
    assert(result === "ok", `session export result=${result}`);
    const text = await page.textContent("[data-testid='session-export-result']");
    return text?.trim().slice(0, 160) ?? "ok";
  },

  async backup() {
    await openUserMenu("backup-open");
    await page.waitForSelector("[data-testid='backup-page']", { timeout: 10_000 });
    await page.click("[data-testid='backup-create']");
    await waitFor(
      async () => {
        const items = page.locator("[data-testid='backup-item']");
        return (await items.count()) >= 1;
      },
      120_000,
      "backup-item",
    );
    const list = await rpcOk("backup_list", {});
    assert((list.backups ?? []).length >= 1, "backup_list 为空");
    const created = list.backups[0];
    await closeOverlay();
    return `backups=${list.backups.length} path=${created.path ?? created.id ?? "?"}`;
  },

  async audit() {
    await openUserMenu("audit-open");
    await page.waitForSelector("[data-testid='audit-page']", { timeout: 10_000 });
    await page.click("[data-testid='audit-query']");
    await waitFor(
      async () => {
        const rows = page.locator("[data-testid='audit-row']");
        return (await rows.count()) >= 1;
      },
      20_000,
      "audit-row",
    );
    const rows = await auditRows({});
    assert(rows.length >= 1, "audit_query 无行");
    const actions = new Set(rows.map((row) => row.action));
    assert(actions.has("session.created"), "审计缺少 session.created");
    await closeOverlay();
    return `rows=${rows.length} actions=${[...actions].slice(0, 6).join(",")}`;
  },

  async settings() {
    await openSettingsTab("general");
    await page.waitForSelector("[data-testid='settings-data-dir']", { timeout: 10_000 });
    const dataDir = await page.textContent("[data-testid='settings-data-dir']");
    assert(dataDir?.includes(workspaces[0].dataDir), `settings data dir=${dataDir}`);
    const toggle = page.locator("[data-testid='settings-backup-reminder']");
    const before = await toggle.isChecked();
    await toggle.click();
    await waitFor(
      async () => (await toggle.isChecked()) !== before,
      10_000,
      "备份提醒开关切换",
    );
    const settings = await rpcOk("settings_get", { key: "backup.reminder" });
    assert(
      settings?.value === !before,
      `settings_get backup.reminder=${JSON.stringify(settings)}`,
    );
    await closeOverlay();
    return `backup.reminder ${before} → ${!before}`;
  },

  async providers() {
    await openSettingsTab("providers");
    await page.waitForSelector("[data-testid='providers-page']", { timeout: 10_000 });
    await page.click("[data-testid='providers-add-custom']");
    await page.waitForSelector("[data-testid='provider-form']", { timeout: 5_000 });
    const name = `M9-12 custom ${Date.now()}`;
    await page.fill("[data-testid='provider-name-input']", name);
    await page.fill("[data-testid='provider-base-url-input']", "https://m9-12.invalid/v1");
    await page.click("[data-testid='provider-form-save']");
    await waitFor(
      async () => {
        const cards = page.locator("[data-testid='provider-card']");
        const count = await cards.count();
        for (let index = 0; index < count; index += 1) {
          if ((await cards.nth(index).textContent())?.includes(name)) return true;
        }
        return false;
      },
      20_000,
      "provider-card（新建）",
    );
    const list = await rpcOk("providers_list", {});
    const providers = Array.isArray(list) ? list : list.providers ?? [];
    assert(
      providers.some((provider) => provider.name === name),
      "providers_list 未包含新建供应商",
    );
    await closeOverlay();
    return `created=${name}`;
  },

  async presets() {
    await openSettingsTab("presets");
    await page.waitForSelector("[data-testid='presets-page']", { timeout: 10_000 });
    const name = `M9-12 preset ${Date.now()}`;
    await page.fill("[data-testid='preset-add-name']", name);
    await page.fill("[data-testid='preset-add-prompt']", "M9-12 E2E preset prompt");
    await page.click("[data-testid='preset-add-submit']");
    await waitFor(
      async () => {
        const items = page.locator("[data-testid='preset-item']");
        const count = await items.count();
        for (let index = 0; index < count; index += 1) {
          if ((await items.nth(index).textContent())?.includes(name)) return true;
        }
        return false;
      },
      20_000,
      "preset-item（新建）",
    );
    const list = await rpcOk("presets_list", {});
    const items = Array.isArray(list) ? list : list.presets ?? [];
    assert(
      items.some((preset) => preset.name === name),
      "presets_list 未包含新建预设",
    );
    await closeOverlay();
    return `created=${name}`;
  },
};

async function run() {
  for (const flow of FLOWS) {
    try {
      const detail = await flowImpl[flow]();
      await screenshot(flow);
      record(flow, true, detail ?? "");
    } catch (error) {
      await screenshot(`failed-${flow}`);
      record(flow, false, error instanceof Error ? error.message : String(error));
      throw new Error(`flow ${flow} 失败：${error instanceof Error ? error.message : error}`);
    }
  }
}

async function cleanup() {
  if (app) {
    try {
      if (traceStarted) {
        await app.context().tracing.stop({ path: tracePath });
      }
    } catch (error) {
      console.warn(`[m9-12-e2e] trace 归档失败：${error.message}`);
    }
    const pid = app.process()?.pid;
    try {
      await Promise.race([app.close(), sleep(8_000)]);
    } catch {
      // 已退出。
    }
    killTree(pid);
    app = null;
  }
  if (process.env.AETHER_M9_12_KEEP_WORKSPACE !== "1") {
    for (const ws of workspaces) {
      safeRemove(ws.root);
    }
  } else {
    for (const ws of workspaces) console.warn(`[m9-12-e2e] 保留工作目录：${ws.root}`);
  }
}

function writeEvidence(ok, error) {
  const evidence = {
    schema: "aether.m9-12.e2e.v1",
    task: "M9-12",
    run_id: runId,
    started_at: startedAt.toISOString(),
    finished_at: new Date().toISOString(),
    platform: process.platform,
    arch: process.arch,
    electron: readElectronVersion(),
    daemon: path.relative(repoRoot, daemonPath),
    bundle: path.relative(repoRoot, bundleDir),
    permission_timeout_ms: permissionTimeoutMs,
    attempts: [
      {
        attempt: 1,
        ok,
        error: error ?? null,
      },
    ],
    flows: FLOWS,
    results,
    artifacts: {
      trace: existsSync(tracePath) ? path.relative(repoRoot, tracePath) : null,
      screenshots: existsSync(screenshotDir)
        ? path.relative(repoRoot, screenshotDir)
        : null,
    },
  };
  mkdirSync(artifactRoot, { recursive: true });
  writeFileSync(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
  return evidence;
}

function readElectronVersion() {
  try {
    const raw = JSON.parse(
      readFileSync(path.join(appRoot, "node_modules", "electron", "package.json"), "utf8"),
    );
    return raw.version ?? null;
  } catch {
    return null;
  }
}

async function main() {
  if (!existsSync(path.join(appRoot, "electron", "dist", "main.cjs"))) {
    throw new Error("缺少 Electron 构建产物：先 pnpm build:desktop");
  }
  if (!existsSync(path.join(appRoot, "dist", "index.html"))) {
    throw new Error("缺少 renderer 构建产物：先 pnpm build:desktop");
  }
  const ws = workspace();
  await launch(ws);
  await run();
}

main()
  .then(async () => {
    await cleanup();
    if (!existsSync(tracePath)) {
      throw new Error(`trace 未归档：${tracePath}（DoD-2 要求 trace/screenshot 归档）`);
    }
    writeEvidence(true, null);
    const passed = results.filter((item) => item.ok).length;
    console.log(`\n[m9-12-e2e] 通过 ${passed}/${FLOWS.length}`);
    console.log(`[m9-12-e2e] trace=${path.relative(repoRoot, tracePath)}`);
    console.log(`[m9-12-e2e] evidence=${path.relative(repoRoot, evidencePath)}`);
    console.log("AETHER_M9_12_E2E PASS");
    process.exit(0);
  })
  .catch(async (error) => {
    fatal = error instanceof Error ? error.stack ?? error.message : String(error);
    console.error("[m9-12-e2e] 致命失败：", error);
    await cleanup();
    writeEvidence(false, fatal);
    console.error(`[m9-12-e2e] evidence=${path.relative(repoRoot, evidencePath)}`);
    process.exit(1);
  });
