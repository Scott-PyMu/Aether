#!/usr/bin/env node
/**
 * M9-07 Electron 壳 E2E（Playwright `_electron`）。
 *
 * 覆盖 DoD：
 *  1. 生产加载 `apps/desktop/dist`（`aether://app`）、无 Tauri 运行时（壳层静态另验）；
 *  2. 窗口 1280×800/min 960×640；preload 五成员；nodeIntegration/contextIsolation/
 *     sandbox/webviewTag；CSP（connect-src/frame-src none）；导航/新窗口拦截；
 *  3. 单实例：第二实例聚焦主窗口并退出，不重复 spawn daemon；
 *  4. 启动顺序（S-00 → daemon）、initialize 10s 硬超时重试 1 次、quick_check
 *     degraded 不退出、A4/DB 无法打开走 S-12（无 S-01/S-01a）、离线导出诊断；
 *  5. 4 条壳本地命令（退出/重启/文件/目录/取消/路径校验）；
 *  6. UI-UX v0.17 S-12 锚点/data-reason/数据目录/手动移动；S-13 锚点（M9-09 触发）。
 *
 * 运行前提：
 *   - `pnpm build:desktop`（renderer + Electron main/preload）；
 *   - `cargo build -p aether-daemon`（真实 daemon 分支）；
 *   - `AETHER_E2E_ELECTRON` 可覆盖 Electron 可执行文件（本机 Electron 45 GUI
 *     崩溃时的环境绕行；CI 必须使用冻结 45.0.0）。
 */
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
const fakeDaemon = path.join(repoRoot, "scripts", "test", "m9-07", "fixtures", "fake-daemon.mjs");
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

const results = [];
const workspaces = [];
const launched = new Set();
let extraPids = [];

function record(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
}

function assert(name, condition, detail = "") {
  if (!condition) {
    throw new Error(detail || name);
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let lastError;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) {
        return value;
      }
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

function workspace(label) {
  // 本机 Chromium 45 的 ProcessSingleton 在受限沙箱下只能写工作区；
  // userData 放 dist-out（已 gitignore），避免 %TEMP%/%APPDATA% 的锁创建失败。
  const base = path.join(repoRoot, "dist-out", "m9-07-e2e");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, `${label}-`));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const registry = path.join(root, "registry");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  mkdirSync(registry, { recursive: true });
  const item = {
    root,
    dataDir,
    userData,
    registry,
    shellLog: path.join(dataDir, "logs", "shell.log"),
  };
  workspaces.push(item);
  return item;
}

function baseEnv(ws) {
  return {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_USER_DATA_DIR: ws.userData,
    AETHER_DATA_DIR: ws.dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(ws.root, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: ws.registry,
  };
}

function fakeEnv(ws, mode) {
  return {
    ...baseEnv(ws),
    AETHERD_BIN: process.execPath,
    AETHERD_EXTRA_ARGS: JSON.stringify([fakeDaemon]),
    AETHER_FAKE_MODE: mode,
    AETHER_FAKE_STATE: path.join(ws.root, "fake-state"),
  };
}

function realEnv(ws) {
  return {
    ...baseEnv(ws),
    AETHERD_BIN: daemonPath,
  };
}

async function launch(env, { useFake = true } = {}) {
  if (useFake && !existsSync(fakeDaemon)) {
    throw new Error(`缺少假 daemon：${fakeDaemon}`);
  }
  if (!useFake && !existsSync(daemonPath)) {
    throw new Error(`缺少 aetherd：${daemonPath}（先 cargo build -p aether-daemon）`);
  }
  const args = [...(noSandbox ? ["--no-sandbox"] : []), repoRoot];
  const app = await _electron.launch({
    executablePath: electronPath,
    args,
    cwd: repoRoot,
    env,
    timeout: 30_000,
  });
  launched.add(app);
  return app;
}

async function snapshot(app) {
  return app.evaluate(() => {
    const hook = globalThis.__aetherShell;
    return hook && typeof hook.snapshot === "function" ? hook.snapshot() : null;
  });
}

async function waitReady(app, timeoutMs = 20_000) {
  return waitFor(
    async () => {
      const state = await snapshot(app);
      return state && state.ready ? state : null;
    },
    timeoutMs,
    "daemon initialize 成功",
  );
}

async function pageWithUrl(app, predicate, timeoutMs = 15_000) {
  return waitFor(
    async () => {
      const pages = app.windows().filter((page) => !page.isClosed());
      for (const page of pages) {
        if (predicate(page.url())) {
          return page;
        }
      }
      return null;
    },
    timeoutMs,
    "目标窗口",
  );
}

async function closeApp(app) {
  try {
    await Promise.race([app.close(), sleep(8_000)]);
  } catch {
    // 已关闭/退出竞态。
  }
  launched.delete(app);
}

function killTree(pid) {
  if (!pid || pid <= 0) {
    return;
  }
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // 已退出。
    }
  }
}

async function patchOpenDialog(app, filePaths) {
  await app.evaluate(
    ({ dialog }, payload) => {
      dialog.showOpenDialog = async () => ({
        canceled: payload === null,
        filePaths: payload === null ? [] : [payload],
      });
    },
    filePaths,
  );
}

async function patchSaveDialog(app, filePath) {
  await app.evaluate(
    ({ dialog }, target) => {
      dialog.showSaveDialog = async () => ({ canceled: false, filePath: target });
    },
    filePath,
  );
}

async function rpcRaw(page, method, params) {
  return page.evaluate(
    ([name, value]) => window.aether.rpc(name, value),
    [method, params],
  );
}

async function rpcOk(page, method, params) {
  const envelope = await rpcRaw(page, method, params);
  if (!envelope || envelope.ok !== true) {
    throw new Error(`RPC ${method} 失败：${JSON.stringify(envelope?.error ?? envelope)}`);
  }
  return envelope.result;
}

function readJsonLines(file) {
  if (!existsSync(file)) {
    return [];
  }
  return readFileSync(file, "utf8")
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

/** Windows 上 Electron 子进程退出后文件句柄可能滞后释放；清理失败不掩盖测试结果。 */
function safeRemove(dir) {
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-07-e2e] 清理临时目录失败（忽略）：${dir} ${error.message}`);
  }
}

// ===== E2E 场景 =====

async function testWindowPreloadAndSecurity() {
  const ws = workspace("window");
  const app = await launch(fakeEnv(ws, "healthy"));
  try {
    const state = await waitReady(app);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    await page.waitForSelector("#root", { timeout: 10_000 }).catch(() => {});

    const windowInfo = await app.evaluate(({ BrowserWindow }) => {
      const win = BrowserWindow.getAllWindows().find((item) => !item.isDestroyed());
      if (!win) return null;
      const bounds = win.getBounds();
      const prefs = win.webContents.getLastWebPreferences();
      return {
        bounds,
        minimum: win.getMinimumSize(),
        prefs: {
          nodeIntegration: prefs.nodeIntegration,
          contextIsolation: prefs.contextIsolation,
          sandbox: prefs.sandbox,
          webSecurity: prefs.webSecurity,
          allowRunningInsecureContent: prefs.allowRunningInsecureContent,
          webviewTag: prefs.webviewTag,
        },
      };
    });
    assert("窗口存在", windowInfo !== null);
    record(
      "DoD-2 窗口 1280×800 / min 960×640",
      windowInfo.bounds.width === 1280 &&
        windowInfo.bounds.height === 800 &&
        windowInfo.minimum[0] === 960 &&
        windowInfo.minimum[1] === 640,
      JSON.stringify({ bounds: windowInfo.bounds, minimum: windowInfo.minimum }),
    );
    record(
      "DoD-2 webPreferences 安全基线 + preload",
      windowInfo.prefs.nodeIntegration === false &&
        windowInfo.prefs.contextIsolation === true &&
        windowInfo.prefs.sandbox === true &&
        windowInfo.prefs.webSecurity === true &&
        windowInfo.prefs.allowRunningInsecureContent === false &&
        windowInfo.prefs.webviewTag === false &&
        typeof state.preloadPath === "string" &&
        state.preloadPath.endsWith("preload.cjs"),
      JSON.stringify({ ...windowInfo.prefs, preload: state.preloadPath }),
    );

    const apiKeys = await page.evaluate(() => Object.keys(window.aether ?? {}).sort());
    record(
      "DoD-2 preload 白名单五成员",
      JSON.stringify(apiKeys) ===
        JSON.stringify(["offEvent", "onEvent", "pathForFile", "rpc", "version"]),
      apiKeys.join(","),
    );
    const nodeGlobals = await page.evaluate(() => ({
      require: typeof window.require,
      process: typeof window.process,
      Buffer: typeof window.Buffer,
      aether: typeof window.aether,
      pathForFile: typeof window.aether?.pathForFile,
    }));
    record(
      "DoD-2 renderer 无 Node/require，pathForFile 可用",
      nodeGlobals.require === "undefined" &&
        nodeGlobals.process === "undefined" &&
        nodeGlobals.Buffer === "undefined" &&
        nodeGlobals.aether === "object" &&
        nodeGlobals.pathForFile === "function",
      JSON.stringify(nodeGlobals),
    );

    const version = await page.evaluate(() => window.aether.version());
    assert("版本对象存在", version && typeof version === "object");
    assert("版本包含 electron/chrome/app", version.electron && version.chrome && version.app);
    record("DoD-1/2 version() 版本信息", true, `${version.app}/${version.electron}`);

    const health = await rpcOk(page, "health", {});
    record(
      "DoD-1 daemon RPC health 可达",
      health && typeof health === "object",
      JSON.stringify(health),
    );

    const unknown = await rpcRaw(page, "definitely_not_a_command", {});
    record(
      "DoD-1 未知命令拒绝（not_implemented）",
      unknown.ok === false && unknown.error?.data?.aether_code === "not_implemented",
      JSON.stringify(unknown),
    );

    // 壳本地命令：文件/目录/取消/路径校验
    const file = path.join(ws.root, "ref.txt");
    writeFileSync(file, "x", "utf8");
    await patchOpenDialog(app, file);
    const picked = await rpcOk(page, "ref_pick", { kind: "file" });
    record(
      "DoD-4/5 ref_pick 文件返回 canonical 路径",
      typeof picked.path === "string" && picked.path.endsWith("ref.txt"),
      picked.path,
    );
    await patchOpenDialog(app, null);
    const cancelled = await rpcOk(page, "ref_pick", { kind: "file" });
    record("DoD-4/5 ref_pick 取消返回 null", cancelled.path === null);

    await patchOpenDialog(app, ws.registry);
    const target = await rpcOk(page, "startup_pick_target", {});
    record(
      "DoD-4/5 startup_pick_target 目录返回 canonical 路径",
      typeof target.target_dir === "string" && target.target_dir.length > 0,
      target.target_dir,
    );

    await patchOpenDialog(app, ws.registry);
    const wrongKind = await rpcRaw(page, "ref_pick", { kind: "file" });
    record(
      "DoD-4/5 ref_pick 路径类型校验（目录当文件 → path_rejected）",
      wrongKind.ok === false && wrongKind.error?.data?.aether_code === "path_rejected",
      JSON.stringify(wrongKind),
    );

    const invalidConfirm = await rpcRaw(page, "app_exit", { confirm: false });
    record(
      "DoD-4/5 app_exit confirm:false 拒绝",
      invalidConfirm.ok === false && invalidConfirm.error?.data?.aether_code === "invalid_value",
      JSON.stringify(invalidConfirm),
    );

    // 启动顺序：窗口先于 daemon
    record(
      "DoD-4 启动顺序：窗口先于 daemon",
      state.windowCreatedAt > 0 &&
        state.daemonSpawnedAt !== null &&
        state.windowCreatedAt <= state.daemonSpawnedAt &&
        state.windowReadyAt !== null &&
        state.windowReadyAt <= state.daemonSpawnedAt,
      JSON.stringify({
        windowCreatedAt: state.windowCreatedAt,
        windowReadyAt: state.windowReadyAt,
        daemonSpawnedAt: state.daemonSpawnedAt,
      }),
    );
    record(
      "DoD-4 initialize 目标 ≤2s",
      state.withinTarget === true && state.initializeMs !== null && state.initializeMs <= 2000,
      `initializeMs=${state.initializeMs}`,
    );

    // CSP：connect-src 'none' 实际阻断 fetch
    const consoleMessages = [];
    page.on("console", (message) => consoleMessages.push(message.text()));
    const fetchResult = await page.evaluate(async () => {
      try {
        await fetch("http://127.0.0.1:9/m9-07-csp");
        return "allowed";
      } catch (error) {
        return `blocked:${error instanceof Error ? error.message : String(error)}`;
      }
    });
    await sleep(300);
    const cspBlocked =
      fetchResult !== "allowed" &&
      consoleMessages.some((text) =>
        /Content Security Policy|Refused to connect|connect-src/i.test(text),
      );
    record("DoD-5 CSP connect-src 'none' 实际生效", cspBlocked, fetchResult);

    // 导航拦截：file: 拒绝，https 交系统浏览器（E2E 只记录）
    const beforeUrl = page.url();
    await page.evaluate(() => {
      window.location.href = "file:///C:/Windows/win.ini";
    });
    await sleep(400);
    record("DoD-5 file: 顶层导航被拒", page.url() === beforeUrl, page.url());

    const opened = await page.evaluate(() => window.open("https://example.com/m9-07-open"));
    await sleep(300);
    const afterOpen = await snapshot(app);
    record(
      "DoD-5 window.open 被拦截并交系统浏览器（记录）",
      opened === null && afterOpen.externalOpens.some((url) => url.includes("m9-07-open")),
      JSON.stringify(afterOpen.externalOpens),
    );

    await page.evaluate(() => {
      window.location.href = "https://example.com/m9-07-nav";
    });
    await sleep(500);
    const afterNav = await snapshot(app);
    record(
      "DoD-5 will-navigate 外链拦截（URL 不变 + 外链记录）",
      page.url() === beforeUrl &&
        afterNav.externalOpens.some((url) => url.includes("m9-07-nav")),
      page.url(),
    );

    // 单实例：第二实例退出 + 不重复 spawn daemon
    const second = spawn(electronPath, [...(noSandbox ? ["--no-sandbox"] : []), repoRoot], {
      cwd: repoRoot,
      env: fakeEnv(ws, "healthy"),
      stdio: "ignore",
    });
    const secondExit = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve("timeout"), 10_000);
      second.on("exit", (code, signal) => {
        clearTimeout(timer);
        resolve(`exit:${code ?? signal}`);
      });
      second.on("error", (error) => {
        clearTimeout(timer);
        resolve(`error:${error.message}`);
      });
    });
    const afterSecond = await waitFor(
      async () => {
        const current = await snapshot(app);
        return current && current.secondInstanceCount >= 1 ? current : null;
      },
      8_000,
      "second-instance 事件",
    );
    const focused = await app.evaluate(({ BrowserWindow }) => {
      const win = BrowserWindow.getAllWindows().find((item) => !item.isDestroyed());
      return win ? win.isFocused() : false;
    });
    record(
      "DoD-3 单实例：第二实例退出、聚焦主窗、daemon 不重复 spawn",
      secondExit.startsWith("exit:0") &&
        afterSecond.daemonSpawnCount === 1 &&
        afterSecond.daemonPid === state.daemonPid &&
        focused === true,
      JSON.stringify({ secondExit, focused, daemonSpawnCount: afterSecond.daemonSpawnCount }),
    );

    // 生产加载：aether://app（非 file/http/Tauri）
    record("DoD-1 生产加载 apps/desktop/dist", page.url() === APP_URL, page.url());
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testStartupPageBeforeDaemon() {
  const ws = workspace("s00");
  const app = await launch(fakeEnv(ws, "delayed"));
  try {
    const first = await app.firstWindow();
    await first.waitForLoadState("domcontentloaded");
    const s00 = await first
      .waitForSelector("[data-testid='startup-loading']", { timeout: 5_000 })
      .then(() => true)
      .catch(() => false);
    record("DoD-4 先创建窗口/S-00 启动页", s00, first.url());
    await waitReady(app, 20_000);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    record("DoD-4 initialize 后加载主 UI", page.url() === APP_URL, page.url());
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function fatalPageFor(app) {
  const page = await pageWithUrl(app, (url) => url.endsWith("/fatal.html"), 20_000);
  await page.waitForFunction(
    () => {
      const root = document.querySelector("[data-testid='shell-fatal']");
      return root !== null && root.dataset.reason !== "";
    },
    { timeout: 10_000 },
  );
  return page;
}

async function assertFatalAnchors(page, expectedReason, ws) {
  const state = await page.evaluate(() => {
    const root = document.querySelector("[data-testid='shell-fatal']");
    const text = (selector) =>
      document.querySelector(`[data-testid='${selector}']`)?.textContent ?? "";
    return {
      reason: root?.dataset.reason ?? "",
      reasonText: text("shell-fatal-reason"),
      logPath: text("shell-fatal-log-path"),
      dataDir: text("shell-fatal-data-dir"),
      manual: text("shell-fatal-manual-move-text"),
      retry: document.querySelector("[data-testid='shell-fatal-retry']") !== null,
      exit: document.querySelector("[data-testid='shell-fatal-exit']") !== null,
      exportButton:
        document.querySelector("[data-testid='shell-fatal-export-diagnostics']") !== null,
      mainPreload: typeof window.aether,
      fatalPreload: typeof window.aetherFatal,
    };
  });
  assert(
    `S-12 原因 ${expectedReason}`,
    state.reason === expectedReason,
    JSON.stringify(state),
  );
  assert("S-12 日志路径", state.logPath.includes("aetherd.log"), state.logPath);
  assert("S-12 数据目录", state.dataDir === ws.dataDir, state.dataDir);
  assert("S-12 手动移动说明", state.manual.includes("AETHER_DATA_DIR"), state.manual);
  assert("S-12 重试/退出/离线导出锚点", state.retry && state.exit && state.exportButton);
  assert("S-12 主 UI preload 未加载", state.mainPreload === "undefined");
  assert("S-12 专用 preload 存在", state.fatalPreload === "object");
  return state;
}

async function testFatalReasonsAndExport() {
  const cases = [
    ["a4_blocked", "bootstrap:sync_dir_blocked"],
    ["data_dir_locked", "bootstrap:data_dir_locked"],
    ["schema_checksum_mismatch", "bootstrap:schema_checksum_mismatch"],
    ["daemon_start_failed", "exit"],
    ["handshake_failed", "handshake"],
  ];
  for (const [expectedReason, mode] of cases) {
    const ws = workspace(`fatal-${expectedReason}`);
    const app = await launch(fakeEnv(ws, mode));
    try {
      const page = await fatalPageFor(app);
      await assertFatalAnchors(page, expectedReason, ws);
      record(`DoD-4 S-12 data-reason=${expectedReason}`, true);

      if (expectedReason === "a4_blocked") {
        const exportPath = path.join(ws.root, "diag.json");
        await patchSaveDialog(app, exportPath);
        await page.click("[data-testid='shell-fatal-export-diagnostics']");
        await page.waitForFunction(
          () => {
            const node = document.querySelector("[data-testid='shell-fatal-export-result']");
            return node?.dataset.result === "ok" || node?.dataset.result === "failed";
          },
          { timeout: 10_000 },
        );
        const result = await page.evaluate(() => {
          const node = document.querySelector("[data-testid='shell-fatal-export-result']");
          return { result: node?.dataset.result ?? "", text: node?.textContent ?? "" };
        });
        const parsed = existsSync(exportPath)
          ? JSON.parse(readFileSync(exportPath, "utf8"))
          : null;
        record(
          "DoD-4/6 S-12 离线导出诊断（不依赖 daemon）",
          result.result === "ok" &&
            parsed?.schema === "aether.shell-diagnostics.v1" &&
            parsed?.fatal?.reason === "a4_blocked" &&
            typeof parsed?.manual_data_dir === "string",
          `${result.result} ${exportPath}`,
        );
      }
    } finally {
      await closeApp(app);
      safeRemove(ws.root);
    }
  }
}

async function testRealDbOpenFailed() {
  const ws = workspace("real-db");
  writeFileSync(path.join(ws.dataDir, "aether.db"), "not a sqlite database", "utf8");
  const app = await launch(realEnv(ws), { useFake: false });
  try {
    const page = await fatalPageFor(app);
    await assertFatalAnchors(page, "db_open_failed", ws);
    record("DoD-4 真实 daemon：DB 无法打开 → S-12 db_open_failed", true);
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testTimeoutRetry() {
  const ws = workspace("timeout");
  const app = await launch(fakeEnv(ws, "timeout_once"));
  try {
    const state = await waitReady(app, 40_000);
    record(
      "DoD-4 initialize 硬超时重试 1 次后成功",
      state.retried === true && state.daemonSpawnCount === 2,
      JSON.stringify({ retried: state.retried, spawns: state.daemonSpawnCount }),
    );
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testFatalRetry() {
  const ws = workspace("retry");
  const app = await launch(fakeEnv(ws, "bootstrap_once"));
  try {
    const fatal = await fatalPageFor(app);
    await assertFatalAnchors(fatal, "db_open_failed", ws);
    await fatal.click("[data-testid='shell-fatal-retry']");
    const state = await waitReady(app, 30_000);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    record(
      "DoD-4 S-12 重试成功 → S-00/主 UI",
      state.ready === true && state.daemonSpawnCount === 2 && page.url() === APP_URL,
      JSON.stringify({ spawns: state.daemonSpawnCount }),
    );
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testDegradedNotFatal() {
  const ws = workspace("degraded");
  const app = await launch(fakeEnv(ws, "degraded"));
  try {
    const state = await waitReady(app);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    const health = await rpcOk(page, "health", {});
    await waitFor(
      async () => {
        const current = await snapshot(app);
        return current.phase === "degraded" ? current : null;
      },
      5_000,
      "daemon.status=degraded",
    );
    const fatalWindows = app.windows().filter((item) => item.url().endsWith("/fatal.html"));
    record(
      "DoD-4 quick_check degraded → 安全模式，不退出/S-12",
      state.ready === true &&
        health.storage_state === "persist_degraded" &&
        fatalWindows.length === 0 &&
        page.url() === APP_URL,
      JSON.stringify({ storage: health.storage_state, fatalWindows: fatalWindows.length }),
    );
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testRealDaemonHealthy() {
  const ws = workspace("real-healthy");
  const app = await launch(realEnv(ws), { useFake: false });
  try {
    const state = await waitReady(app, 40_000);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    const health = await waitFor(
      async () => {
        try {
          return await rpcOk(page, "health", {});
        } catch {
          return null;
        }
      },
      30_000,
      "真实 daemon health（预热完成）",
    );
    const list = await waitFor(
      async () => {
        try {
          return await rpcOk(page, "session_list", {});
        } catch {
          return null;
        }
      },
      15_000,
      "真实 daemon session_list",
    );
    record(
      "DoD-4 真实 daemon initialize/health/session_list",
      state.ready === true && health && typeof health === "object" && list && typeof list === "object",
      JSON.stringify({ storage: health.storage_state ?? null, daemon: state.daemonPid }),
    );
  } finally {
    await closeApp(app);
    safeRemove(ws.root);
  }
}

async function testDataDirLocked() {
  const ws = workspace("locked");
  const holder = spawn(daemonPath, ["--stdio"], {
    env: realEnv(ws),
    stdio: ["pipe", "pipe", "pipe"],
  });
  try {
    await sleep(1_500);
    if (holder.exitCode !== null) {
      throw new Error(`锁持有 daemon 提前退出：${holder.exitCode}`);
    }
    const app = await launch(realEnv(ws), { useFake: false });
    try {
      const page = await fatalPageFor(app);
      await assertFatalAnchors(page, "data_dir_locked", ws);
      record("DoD-4 真实 daemon：数据目录锁被占用 → S-12 data_dir_locked", true);
    } finally {
      await closeApp(app);
    }
  } finally {
    killTree(holder.pid);
    holder.stdin?.destroy();
    safeRemove(ws.root);
  }
}

async function testAppExit() {
  const ws = workspace("exit");
  const app = await launch(fakeEnv(ws, "healthy"));
  let closed = false;
  try {
    await waitReady(app);
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    const response = await rpcOk(page, "app_exit", { confirm: true });
    assert("app_exit 响应", response?.exiting === true, JSON.stringify(response));
    await Promise.race([
      app.waitForEvent("close").then(() => {
        closed = true;
      }),
      new Promise((resolve) => app.process().once("exit", resolve)).then(() => {
        closed = true;
      }),
      sleep(10_000),
    ]);
    const log = readJsonLines(ws.shellLog);
    record(
      "DoD-4/5 app_exit：响应 + 进程退出 + 关闭序列日志",
      response.exiting === true &&
        closed &&
        log.some((line) => line.event === "app.exit.requested") &&
        log.some((line) => line.event === "app.shutdown.done"),
      JSON.stringify({ closed }),
    );
  } finally {
    if (!closed) {
      await closeApp(app);
    }
    safeRemove(ws.root);
  }
}

async function testAppRestart() {
  const ws = workspace("restart");
  const app = await launch(fakeEnv(ws, "healthy"));
  try {
    const state = await waitReady(app);
    const firstPid = state.pid;
    const page = await pageWithUrl(app, (url) => url === APP_URL);
    const response = await rpcOk(page, "app_restart", { confirm: true });
    assert("app_restart 响应", response?.restarting === true, JSON.stringify(response));
    await Promise.race([
      app.waitForEvent("close"),
      new Promise((resolve) => app.process().once("exit", resolve)),
      sleep(10_000),
    ]);
    const secondStart = await waitFor(
      () => {
        const starts = readJsonLines(ws.shellLog).filter((line) => line.event === "app.start");
        return starts.find((line) => line.pid !== firstPid) ?? null;
      },
      30_000,
      "重启后的 app.start",
    );
    extraPids.push(secondStart.pid);
    record(
      "DoD-4/5 app_restart：relaunch 后新进程启动",
      secondStart.pid !== firstPid && response.restarting === true,
      `first=${firstPid} second=${secondStart.pid}`,
    );
  } finally {
    await closeApp(app);
    for (const pid of extraPids) {
      killTree(pid);
    }
    extraPids = [];
    safeRemove(ws.root);
  }
}

async function testKeychainPageAnchors() {
  // S-13 触发属 M9-09；这里只验证最小 renderer 锚点与专用 preload 约定。
  const html = readFileSync(
    path.join(appRoot, "electron", "keychain", "keychain.html"),
    "utf8",
  );
  const anchors = [
    "keychain-reauth",
    "keychain-reauth-reason",
    "keychain-reauth-retry",
    "keychain-reauth-passphrase",
    "keychain-reauth-skip",
    "keychain-reauth-notice",
  ];
  const missing = anchors.filter((anchor) => !html.includes(anchor));
  record(
    "DoD-6 UI-UX v0.17 S-13 锚点齐备（M9-09 触发）",
    missing.length === 0,
    missing.join(","),
  );
}

async function main() {
  if (!existsSync(path.join(appRoot, "electron", "dist", "main.cjs"))) {
    throw new Error("缺少 Electron 构建产物：先 pnpm build:desktop");
  }
  console.log(`[m9-07-e2e] Electron：${electronPath}`);
  console.log(`[m9-07-e2e] aetherd：${daemonPath}`);

  const scenarios = [
    ["窗口/preload/安全/壳本地命令/单实例", testWindowPreloadAndSecurity],
    ["S-00 先于 daemon", testStartupPageBeforeDaemon],
    ["S-12 原因矩阵 + 离线导出", testFatalReasonsAndExport],
    ["真实 DB 无法打开", testRealDbOpenFailed],
    ["initialize 超时重试", testTimeoutRetry],
    ["S-12 重试成功", testFatalRetry],
    ["degraded 安全模式不退出", testDegradedNotFatal],
    ["真实 daemon 健康路径", testRealDaemonHealthy],
    ["数据目录锁", testDataDirLocked],
    ["app_exit", testAppExit],
    ["app_restart", testAppRestart],
    ["S-13 锚点", testKeychainPageAnchors],
  ];

  let failed = 0;
  for (const [name, fn] of scenarios) {
    try {
      await fn();
    } catch (error) {
      failed += 1;
      record(name, false, error instanceof Error ? error.message : String(error));
    }
  }

  for (const app of [...launched]) {
    await closeApp(app);
  }
  for (const pid of extraPids) {
    killTree(pid);
  }
  extraPids = [];
  for (const ws of workspaces) {
    safeRemove(ws.root);
  }

  const passed = results.filter((item) => item.ok).length;
  console.log(`\n[m9-07-e2e] 通过 ${passed}/${results.length}`);
  if (failed > 0 || passed !== results.length) {
    console.error("[m9-07-e2e] FAIL");
    process.exit(1);
  }
  console.log("AETHER_M9_07_E2E PASS");
  process.exit(0);
}

main().catch((error) => {
  console.error("[m9-07-e2e] 致命失败：", error);
  for (const app of [...launched]) {
    void closeApp(app);
  }
  for (const pid of extraPids) {
    killTree(pid);
  }
  process.exit(1);
});
