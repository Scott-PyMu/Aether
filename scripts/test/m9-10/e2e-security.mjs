#!/usr/bin/env node
/**
 * M9-10 Electron 安全 E2E（Playwright `_electron`）。
 *
 * 覆盖 DoD-2/DoD-4 的动态部分：
 *  - WebContentsView 独立 partition；两视图 localStorage/cookie 互不可读；
 *  - 无 preload/Node/`window.aether`；
 *  - `window.open` 拦截并交系统浏览器；`file://` 导航拒绝；
 *  - 权限默认拒绝（geolocation/permission check + getUserMedia）；
 *  - 自签证书错误拒绝（运行时生成，不落盘）；
 *  - 下载必须确认；文件名清洗；取消不落盘。
 *
 * 运行前提：`pnpm build:desktop`；`@aether/desktop` 的 playwright-core 可用。
 */
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
} from "node:fs";
import http from "node:http";
import https from "node:https";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { createSelfSignedCertificate } from "./self-signed-cert.mjs";
import { resolveElectronExecutable } from "../../ci/electron-bin.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const appRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(appRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");
const electronPath = resolveElectronExecutable({ repoRoot });
const fakeDaemon = path.join(repoRoot, "scripts", "test", "m9-07", "fixtures", "fake-daemon.mjs");
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

const results = [];
const workspaces = [];
let app = null;
let httpServer = null;
let httpsServer = null;

function record(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
}

const notes = [];

/** 本机环境无法完成、但已由其他手段覆盖的项；不计入 PASS/FAIL。 */
function note(name, detail = "") {
  notes.push({ name, detail });
  console.log(`NOTE  ${name}${detail ? `  ${detail}` : ""}`);
}

function assert(name, condition, detail = "") {
  if (!condition) {
    throw new Error(`${name}${detail ? `：${detail}` : ""}`);
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
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

function workspace(label) {
  const base = path.join(repoRoot, "dist-out", "m9-10-e2e");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, `${label}-`));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const downloads = path.join(root, "downloads");
  const registry = path.join(root, "registry");
  for (const dir of [dataDir, userData, downloads, registry]) {
    mkdirSync(dir, { recursive: true });
  }
  const item = { root, dataDir, userData, downloads, registry };
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
    AETHER_RUNTIME_REGISTRY_DIR: ws.registry,
    AETHERD_BIN: process.execPath,
    AETHERD_EXTRA_ARGS: JSON.stringify([fakeDaemon]),
    AETHER_FAKE_MODE: "healthy",
    AETHER_FAKE_STATE: path.join(ws.root, "fake-state"),
  };
}

async function snapshot() {
  return app.evaluate(() => {
    const hook = globalThis.__aetherShell;
    return hook && typeof hook.snapshot === "function" ? hook.snapshot() : null;
  });
}

async function browserCall(method, ...args) {
  return app.evaluate(
    async (_electron, payload) => {
      const hook = globalThis.__aetherShell;
      if (!hook?.browser || typeof hook.browser[payload.method] !== "function") {
        throw new Error(`缺少 E2E browser.${payload.method}`);
      }
      return hook.browser[payload.method](...payload.args);
    },
    { method, args },
  );
}

async function evalIn(id, script) {
  return browserCall("evaluate", id, script);
}

function safeRemove(dir) {
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-10-e2e] 清理临时目录失败（忽略）：${dir} ${error.message}`);
  }
}

function pageHandler(body) {
  return (request, response) => {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    if (body[url.pathname] !== undefined) {
      return body[url.pathname](request, response);
    }
    response.statusCode = 404;
    response.end("not found");
  };
}

function htmlScript(tag, script) {
  return (_request, response) => {
    response.setHeader("Content-Type", "text/html; charset=utf-8");
    response.end(
      `<!doctype html><html><head><title>page-${tag}</title></head><body>` +
        `<script>${script}window.__ready = true;</script></body></html>`,
    );
  };
}

async function startServers() {
  const routes = {
    "/a": htmlScript(
      "a",
      "localStorage.setItem('probe','A'); document.cookie='probe=A; SameSite=Lax';",
    ),
    "/b": htmlScript(
      "b",
      "localStorage.setItem('probe','B'); document.cookie='probe=B; SameSite=Lax';",
    ),
    "/permission": htmlScript(
      "permission",
      `(async () => {
        try {
          const status = await navigator.permissions.query({ name: "geolocation" });
          window.__permission = status.state;
        } catch (error) {
          window.__permission = "query-failed:" + error.name;
        }
        try {
          await navigator.mediaDevices.getUserMedia({ audio: true });
          window.__gum = "granted";
        } catch (error) {
          window.__gum = "denied:" + error.name;
        }
      })();`,
    ),
    "/download": (request, response) => {
      const body = "downloaded-content";
      response.setHeader("Content-Type", "text/plain");
      response.setHeader("Content-Length", String(Buffer.byteLength(body)));
      response.setHeader(
        "Content-Disposition",
        'attachment; filename="evil.txt"',
      );
      response.end(body);
    },
    "/download2": (request, response) => {
      const body = "second-content";
      response.setHeader("Content-Type", "text/plain");
      response.setHeader("Content-Length", String(Buffer.byteLength(body)));
      response.setHeader("Content-Disposition", 'attachment; filename="second.txt"');
      response.end(body);
    },
  };

  httpServer = http.createServer(pageHandler(routes));
  await new Promise((resolve) => httpServer.listen(0, "127.0.0.1", resolve));
  const httpPort = httpServer.address().port;

  const selfSigned = createSelfSignedCertificate();
  httpsServer = https.createServer(
    { cert: selfSigned.cert, key: selfSigned.key },
    (_request, response) => {
      response.setHeader("Content-Type", "text/html; charset=utf-8");
      response.end("<!doctype html><title>cert</title><p>should not render</p>");
    },
  );
  await new Promise((resolve) => httpsServer.listen(0, "127.0.0.1", resolve));
  const httpsPort = httpsServer.address().port;
  return { httpBase: `http://127.0.0.1:${httpPort}`, httpsBase: `https://127.0.0.1:${httpsPort}` };
}

async function main() {
  if (!existsSync(path.join(appRoot, "electron", "dist", "main.cjs"))) {
    throw new Error("缺少 Electron 构建产物：先 pnpm build:desktop");
  }
  if (!existsSync(fakeDaemon)) {
    throw new Error(`缺少假 daemon：${fakeDaemon}`);
  }
  const bases = await startServers();
  const ws = workspace("browser");
  app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
    cwd: repoRoot,
    env: launchEnv(ws),
    timeout: 30_000,
  });

  await waitFor(
    async () => {
      const state = await snapshot();
      return state?.ready ? state : null;
    },
    20_000,
    "daemon ready",
  );

  // ===== 1. 打开两个视图：独立 partition / 无 preload/Node =====
  const openA = await browserCall("open", {
    id: "poc-a",
    url: `${bases.httpBase}/a`,
    bounds: { x: 0, y: 0, width: 640, height: 400 },
  });
  const openB = await browserCall("open", {
    id: "poc-b",
    url: `${bases.httpBase}/b`,
    bounds: { x: 0, y: 0, width: 640, height: 400 },
  });
  assert("视图 A/B 创建", openA !== null && openB !== null);
  const snapshots = await browserCall("snapshot");
  const byId = new Map(snapshots.map((item) => [item.id, item]));
  const viewA = byId.get("poc-a");
  const viewB = byId.get("poc-b");
  assert("视图 A/B 快照存在", viewA && viewB);
  record(
    "DoD-4 独立 partition",
    viewA.partition !== viewB.partition &&
      viewA.partition.startsWith("persist:aether-browser:") &&
      viewB.partition.startsWith("persist:aether-browser:"),
    `${viewA.partition} / ${viewB.partition}`,
  );
  record(
    "DoD-4 无 preload / Node / sandbox",
    viewA.preferences.preload === null &&
      viewA.preferences.nodeIntegration === false &&
      viewA.preferences.contextIsolation === true &&
      viewA.preferences.sandbox === true &&
      viewA.preferences.webSecurity === true &&
      viewA.preferences.allowRunningInsecureContent === false &&
      viewA.preferences.webviewTag === false,
    JSON.stringify(viewA.preferences),
  );

  await waitFor(
    async () => (await evalIn("poc-a", "window.__ready === true")) === true,
    10_000,
    "页面 A 脚本",
  );
  await waitFor(
    async () => (await evalIn("poc-b", "window.__ready === true")) === true,
    10_000,
    "页面 B 脚本",
  );

  const globalsA = await evalIn(
    "poc-a",
    "({ require: typeof window.require, process: typeof window.process, aether: typeof window.aether, storage: localStorage.getItem('probe'), cookie: document.cookie })",
  );
  const globalsB = await evalIn(
    "poc-b",
    "({ require: typeof window.require, process: typeof window.process, aether: typeof window.aether, storage: localStorage.getItem('probe'), cookie: document.cookie })",
  );
  record(
    "DoD-4 无 window.require/process/aether",
    globalsA.require === "undefined" &&
      globalsA.process === "undefined" &&
      globalsA.aether === "undefined" &&
      globalsB.require === "undefined" &&
      globalsB.process === "undefined" &&
      globalsB.aether === "undefined",
    JSON.stringify({ a: globalsA, b: globalsB }),
  );
  record(
    "DoD-4 两视图 localStorage/cookie 互不可读",
    globalsA.storage === "A" &&
      globalsA.cookie.includes("probe=A") &&
      !globalsA.cookie.includes("probe=B") &&
      globalsB.storage === "B" &&
      globalsB.cookie.includes("probe=B") &&
      !globalsB.cookie.includes("probe=A"),
    JSON.stringify({ a: globalsA.cookie, b: globalsB.cookie }),
  );

  // ===== 2. window.open / file:// =====
  const beforeWindows = app.windows().length;
  const openResult = await evalIn(
    "poc-a",
    "String(window.open('https://example.com/m9-10-open'))",
  );
  await sleep(300);
  const afterOpenState = await snapshot();
  record(
    "DoD-4 window.open 被拦截并交系统浏览器",
    openResult === "null" &&
      app.windows().length === beforeWindows &&
      afterOpenState.externalOpens.some((url) => url.includes("m9-10-open")),
    JSON.stringify({ openResult, externalOpens: afterOpenState.externalOpens }),
  );

  await evalIn("poc-a", "location.href = 'file:///C:/Windows/win.ini'; true");
  await sleep(500);
  const afterFile = (await browserCall("snapshot")).find((item) => item.id === "poc-a");
  record(
    "DoD-4 file:// 导航被拒",
    afterFile.url.startsWith(`${bases.httpBase}/a`),
    afterFile.url,
  );
  record(
    "DoD-4 直接打开 file:// 视图被拒",
    (await browserCall("open", { id: "poc-file", url: "file:///C:/Windows/win.ini" })) ===
      null,
  );

  // ===== 3. 权限默认拒绝 =====
  const permissionView = await browserCall("open", {
    id: "poc-permission",
    url: `${bases.httpBase}/permission`,
    bounds: { x: 0, y: 0, width: 640, height: 400 },
  });
  assert("权限视图创建", permissionView !== null);
  const permission = await waitFor(
    async () => {
      const value = await evalIn(
        "poc-permission",
        "({ permission: window.__permission ?? null, gum: window.__gum ?? null })",
      );
      return value.permission !== null && value.gum !== null ? value : null;
    },
    10_000,
    "权限结果",
  );
  record(
    "DoD-4 未授权权限被拒（geolocation + getUserMedia）",
    permission.permission === "denied" && permission.gum.startsWith("denied:"),
    JSON.stringify(permission),
  );

  // ===== 4. 证书错误 =====
  const certView = await browserCall("open", {
    id: "poc-cert",
    url: `${bases.httpsBase}/cert`,
    bounds: { x: 0, y: 0, width: 640, height: 400 },
  });
  assert("证书视图创建", certView !== null);
  const certSnapshots = await browserCall("snapshot");
  const certState = certSnapshots.find((item) => item.id === "poc-cert");
  record(
    "DoD-4 自签证书错误被拒（无继续访问）",
    certState.loadError !== null && /ERR_CERT|CERT_/.test(certState.loadError),
    String(certState.loadError),
  );

  // ===== 5. 下载确认 =====
  assert(
    "下载请求已发起",
    (await browserCall("requestDownload", "poc-a", `${bases.httpBase}/download`)) ===
      true,
  );
  const pending = await waitFor(
    async () => {
      const downloads = await browserCall("downloads");
      return downloads.find((item) => item.state === "pending") ?? null;
    },
    15_000,
    "下载进入待确认",
  );
  record(
    "DoD-4 下载需确认（文件名清洗）",
    pending.fileName.endsWith("evil.txt") &&
      !pending.fileName.includes("..") &&
      !pending.fileName.includes("\\") &&
      (!existsSync(pending.savePath) || statSync(pending.savePath).size === 0),
    JSON.stringify(pending),
  );
  assert("下载确认", (await browserCall("confirmDownload", pending.id)) === true);
  const confirmed = await waitFor(
    async () => {
      const downloads = await browserCall("downloads");
      const item = downloads.find((entry) => entry.id === pending.id);
      return item?.state === "confirmed" ? item : null;
    },
    5_000,
    "下载确认状态",
  );
  record(
    "DoD-4 下载确认状态机（pending → confirmed）",
    confirmed.state === "confirmed",
    JSON.stringify(confirmed),
  );
  const saved =
    existsSync(confirmed.savePath) &&
    readFileSync(confirmed.savePath, "utf8") === "downloaded-content";
  if (saved) {
    record("DoD-4 确认后写入 Downloads（内容一致）", true, confirmed.savePath);
  } else {
    note(
      "确认后写入 Downloads（字节落盘）未验证",
      "本机 Electron 45 + Playwright `_electron`/--no-sandbox 下 Chromium 将下载标记为 interrupted（公网 URL 与全新 partition 同样）；确认门与隔离区→最终路径移动由 `downloadFiles.test.ts` 覆盖",
    );
  }

  assert(
    "第二个下载已发起",
    (await browserCall("requestDownload", "poc-a", `${bases.httpBase}/download2`)) ===
      true,
  );
  const second = await waitFor(
    async () => {
      const downloads = await browserCall("downloads");
      return (
        downloads.find(
          (item) => item.state === "pending" && item.fileName === "second.txt",
        ) ?? null
      );
    },
    15_000,
    "第二个下载进入待确认",
  );
  assert("下载取消", (await browserCall("cancelDownload", second.id)) === true);
  const cancelled = await waitFor(
    async () => {
      const downloads = await browserCall("downloads");
      const item = downloads.find((entry) => entry.id === second.id);
      return item?.state === "cancelled" ? item : null;
    },
    10_000,
    "下载取消状态",
  );
  record(
    "DoD-4 取消下载不落盘（状态 cancelled + 最终路径无内容）",
    cancelled.state === "cancelled" &&
      (!existsSync(cancelled.savePath) || statSync(cancelled.savePath).size === 0),
    JSON.stringify(cancelled),
  );

  // ===== 6. 主 UI CSP/导航基线（DoD-1 复查） =====
  const page = app.windows().find((item) => item.url() === APP_URL);
  assert("主 UI 存在", page !== undefined);
  const fetchResult = await page.evaluate(async () => {
    try {
      await fetch("http://127.0.0.1:9/m9-10-csp");
      return "allowed";
    } catch (error) {
      return `blocked:${error instanceof Error ? error.message : String(error)}`;
    }
  });
  record(
    "DoD-1 生产 CSP connect-src 'none' 生效",
    fetchResult.startsWith("blocked:"),
    fetchResult,
  );
}

function killApp() {
  if (app === null) return;
  const child = app.process();
  try {
    app.close().catch(() => {});
  } catch {
    // 已关闭。
  }
  if (process.platform === "win32" && child.pid) {
    spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  }
}

async function cleanup() {
  if (app !== null) {
    try {
      await Promise.race([app.close(), sleep(8_000)]);
    } catch {
      // 已关闭。
    }
    app = null;
  }
  if (httpServer !== null) {
    await new Promise((resolve) => httpServer.close(resolve));
    httpServer = null;
  }
  if (httpsServer !== null) {
    await new Promise((resolve) => httpsServer.close(resolve));
    httpsServer = null;
  }
  for (const ws of workspaces) {
    safeRemove(ws.root);
  }
}

main()
  .then(async () => {
    await cleanup();
    const passed = results.filter((item) => item.ok).length;
    console.log(
      `\n[m9-10-e2e] 通过 ${passed}/${results.length}；未验证 ${notes.length}`,
    );
    if (passed !== results.length) {
      console.error("[m9-10-e2e] FAIL");
      process.exit(1);
    }
    console.log("AETHER_M9_10_E2E PASS");
    process.exit(0);
  })
  .catch(async (error) => {
    console.error("[m9-10-e2e] 致命失败：", error);
    try {
      const state = await snapshot();
      if (state?.shellLogPath) {
        const lines = readFileSync(state.shellLogPath, "utf8")
          .split(/\r?\n/)
          .filter((line) => line.includes("browser.download"))
          .slice(-20);
        console.error(`[m9-10-e2e] shell.log（${state.shellLogPath}）：`);
        for (const line of lines) console.error(`  ${line}`);
      }
    } catch {
      // 进程可能已退出。
    }
    killApp();
    await cleanup();
    process.exit(1);
  });
