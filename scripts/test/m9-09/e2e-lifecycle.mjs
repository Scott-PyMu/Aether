#!/usr/bin/env node
/**
 * M9-09 生命周期 E2E（Playwright `_electron`）。
 *
 * 覆盖 DoD：
 *  - daemon `kill -9` → 保留窗口 + 1s 退避重启 → 恢复后 `session_list` 成功；
 *  - 60s 内 ≥5 次停止 → 停止自动重启（熔断），UI 出现核心横幅；
 *  - daemon 收到 `shutdown` 后卡死 → main ≤10s 整树回收（父 + 子进程均消失）。
 *
 * 运行前提：`pnpm build:desktop`、`cargo build -p aether-daemon`。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
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
const stuckDaemon = path.join(repoRoot, "scripts", "test", "m9-09", "fixtures", "stuck-daemon.mjs");
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

const results = [];
const workspaces = [];
const launched = new Set();

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
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

function attachPageDiagnostics(page) {
  page.on("pageerror", (error) => {
    console.log(`[m9-09-e2e] renderer pageerror: ${error.message}`);
  });
  page.on("console", (message) => {
    if (message.type() === "error") {
      console.log(`[m9-09-e2e] renderer console.error: ${message.text()}`);
    }
  });
  page.on("crash", () => {
    console.log("[m9-09-e2e] renderer crashed");
  });
}

function workspace(label) {  const base = path.join(repoRoot, "dist-out", "m9-09-e2e");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, `${label}-`));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const registry = path.join(root, "registry");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  mkdirSync(registry, { recursive: true });
  const item = { root, dataDir, userData, registry };
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
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
  };
}

function realEnv(ws) {
  return { ...baseEnv(ws), AETHERD_BIN: daemonPath };
}

function fakeEnv(ws, mode, stateFile) {
  return {
    ...baseEnv(ws),
    AETHERD_BIN: process.execPath,
    AETHERD_EXTRA_ARGS: JSON.stringify([fakeDaemon]),
    AETHER_FAKE_MODE: mode,
    AETHER_FAKE_REQUEST_LOG: path.join(ws.root, "requests.log"),
    ...(stateFile ? { AETHER_FAKE_STATE: stateFile } : {}),
  };
}

function stuckEnv(ws, pidFile) {
  return {
    ...baseEnv(ws),
    AETHERD_BIN: process.execPath,
    AETHERD_EXTRA_ARGS: JSON.stringify([stuckDaemon]),
    AETHER_STUCK_MODE: "ignore-shutdown",
    AETHER_STUCK_CHILD_PID_FILE: pidFile,
  };
}

async function launch(env, label) {
  const args = [...(noSandbox ? ["--no-sandbox"] : []), repoRoot];
  const app = await _electron.launch({
    executablePath: electronPath,
    args,
    cwd: repoRoot,
    env,
    timeout: 30_000,
  });
  app.on("window", (page) => {
    attachPageDiagnostics(page);
  });
  for (const page of app.windows()) {
    attachPageDiagnostics(page);
  }
  launched.add(app);
  const state = await waitFor(
    async () => {
      const snapshot = await app.evaluate(() => globalThis.__aetherShell?.snapshot?.() ?? null);
      return snapshot?.ready ? snapshot : null;
    },
    30_000,
    `${label} daemon ready`,
  );
  return { app, state };
}

async function snapshot(app) {
  return app.evaluate(() => globalThis.__aetherShell?.snapshot?.() ?? null);
}

async function page(app) {
  return waitFor(
    async () => {
      const pages = app.windows().filter((item) => !item.isClosed());
      return pages.find((item) => item.url() === APP_URL) ?? null;
    },
    15_000,
    "主窗口",
  );
}

async function rpcOk(target, method, params) {
  const envelope = await target.evaluate(
    ([name, value]) => window.aether.rpc(name, value),
    [method, params],
  );
  if (!envelope || envelope.ok !== true) {
    throw new Error(`RPC ${method} 失败：${JSON.stringify(envelope?.error ?? envelope)}`);
  }
  return envelope.result;
}

/** 预热窗口内 `core_not_ready` 是预期错误：轮询到成功。 */
async function rpcOkEventually(target, method, params, timeoutMs = 20_000) {
  return waitFor(
    async () => {
      const envelope = await target.evaluate(
        ([name, value]) => window.aether.rpc(name, value),
        [method, params],
      );
      if (envelope?.ok === true) {
        return envelope.result;
      }
      const code = envelope?.error?.data?.aether_code;
      if (code === "core_not_ready") {
        return null;
      }
      throw new Error(`RPC ${method} 失败：${JSON.stringify(envelope?.error ?? envelope)}`);
    },
    timeoutMs,
    `${method} 成功`,
  );
}

async function closeApp(app) {
  let pid;
  try {
    pid = app.process()?.pid;
  } catch {
    pid = undefined;
  }
  try {
    await Promise.race([app.close(), sleep(8_000)]);
  } catch {
    // 已退出/竞态。
  }
  // 兜底：避免 Playwright 连接残留子进程导致 Node 不退出（不掩盖产品断言）。
  killTree(pid);
  if (typeof pid === "number" && pid > 0) {
    const deadline = Date.now() + 5_000;
    while (pidAlive(pid) && Date.now() < deadline) {
      await sleep(100);
    }
  }
  launched.delete(app);
  await sleep(300);
}

function pidAlive(pid) {
  if (!pid || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

function killTree(pid) {
  if (!pid || pid <= 0 || !pidAlive(pid)) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
      stdio: "ignore",
      timeout: 5_000,
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
  } catch {
    // Windows 文件句柄滞后；清理失败不掩盖结果。
  }
}

async function scenarioUiTimingAndSessionCreate() {
  const ws = workspace("timing");
  const { app, state } = await launch(fakeEnv(ws, "healthy"), "timing");
  try {
    const windowMs = (state.windowReadyAt ?? state.startedAt) - state.startedAt;
    record(
      "DoD-1 窗口可见 ≤1.5s",
      windowMs <= 1_500,
      `windowMs=${windowMs}`,
    );
    record(
      "DoD-1 initialize 目标 ≤2s",
      state.initializeMs <= 2_000,
      `initializeMs=${state.initializeMs} withinTarget=${state.withinTarget}`,
    );
    const target = await page(app);
    const uiMs = await waitFor(
      async () => {
        const next = await snapshot(app);
        return next.uiReadyAt ? (next.uiReadyAt ?? 0) - next.startedAt : null;
      },
      10_000,
      "首个 health 成功（UI ready）",
    );
    record("DoD-1 UI ready ≤5s", uiMs <= 5_000, `uiReadyMs=${uiMs}`);
    await rpcOk(target, "session_create", { title: "M9-09 timing" });
    const after = await waitFor(
      async () => {
        const next = await snapshot(app);
        return next.sessionCreateMs !== null ? next : null;
      },
      5_000,
      "session_create 计时",
    );
    record(
      "DoD-1 会话创建 <2s",
      after.sessionCreateMs < 2_000,
      `sessionCreateMs=${after.sessionCreateMs}`,
    );
    const requestLog = path.join(ws.root, "requests.log");
    if (existsSync(requestLog)) {
      const methods = [
        ...new Set(readFileSync(requestLog, "utf8").split(/\r?\n/).filter(Boolean)),
      ];
      console.log(`[m9-09-e2e] timing daemon 方法：${methods.join(",")}`);
    }
  } finally {
    if (launched.has(app)) {
      await closeApp(app);
    }
  }
}

async function scenarioRestartAfterKill() {
  const ws = workspace("restart");
  const { app, state } = await launch(realEnv(ws), "restart");
  try {
    const target = await page(app);
    const before = await rpcOkEventually(target, "session_list", {});
    assert("kill-9 前 session_list 可用", Array.isArray(before));

    process.kill(state.daemonPid, "SIGKILL");
    const recovered = await waitFor(
      async () => {
        const next = await snapshot(app);
        return next?.ready && next.daemonPid && next.daemonPid !== state.daemonPid
          ? next
          : null;
      },
      30_000,
      "daemon 退避重启",
    );
    record(
      "DoD-3 daemon kill -9 → 1s 退避重启并恢复",
      recovered.daemonSpawnCount >= 2 && recovered.ready === true,
      `spawns=${recovered.daemonSpawnCount} newPid=${recovered.daemonPid}`,
    );
    record(
      "DoD-3 重启保留窗口（不重建 BrowserWindow）",
      recovered.windows?.main === true && recovered.rendererUrl === APP_URL,
      `windows.main=${recovered.windows?.main} url=${recovered.rendererUrl}`,
    );
    const after = await rpcOkEventually(target, "session_list", {});
    record(
      "DoD-3 恢复后 session_list 同步",
      Array.isArray(after),
      `sessions=${Array.isArray(after) ? after.length : "non-array"}`,
    );
  } finally {
    if (launched.has(app)) {
      await closeApp(app);
    }
  }
}

async function scenarioCrashLoopPauses() {
  const ws = workspace("crash-loop");
  const stateFile = path.join(ws.root, "fake-state");
  const { app } = await launch(fakeEnv(ws, "crash_loop", stateFile), "crash-loop");
  try {
    const paused = await waitFor(
      async () => {
        const next = await snapshot(app);
        return next?.restart?.paused ? next : null;
      },
      60_000,
      "60s 内 5 次停止 → 熔断",
    );
    record(
      "DoD-3 60s 内 ≥5 次停止 → 停止自动重启",
      paused.restart.paused === true &&
        paused.restart.stopsInWindow >= 5 &&
        paused.restart.attempt >= 3,
      `stops=${paused.restart.stopsInWindow} attempt=${paused.restart.attempt} spawns=${paused.daemonSpawnCount}`,
    );
    let target = await page(app);
    const banner = await waitFor(
      async () => {
        if (target.isClosed()) {
          target = await page(app);
        }
        const found = await target.$(
          '[data-testid="core-not-ready-banner"], [data-testid="core-unresponsive"]',
        );
        return found ?? null;
      },
      35_000,
      "核心横幅",
    ).catch(() => null);
    if (banner === null) {
      const debug = await target.evaluate(() => ({
        url: location.href,
        readyState: document.readyState,
        hasAether: typeof window.aether === "object" && window.aether !== null,
        rootChildren: document.getElementById("root")?.childElementCount ?? -1,
        body: document.body?.innerText.slice(0, 500) ?? "(no body)",
        html: document.documentElement?.outerHTML.slice(0, 800) ?? "(no html)",
      }));
      console.log(`[m9-09-e2e] 熔断后页面：${JSON.stringify(debug)}`);
      console.log(`[m9-09-e2e] 熔断后快照：${JSON.stringify(await snapshot(app))}`);
      const shellLog = path.join(ws.dataDir, "logs", "shell.log");
      if (existsSync(shellLog)) {
        const all = readFileSync(shellLog, "utf8").split(/\r?\n/);
        const interesting = all
          .filter((line) => /renderer|gone|child_process|window|fatal/i.test(line))
          .slice(-30);
        console.log(`[m9-09-e2e] shell.log 关键行：\n${interesting.join("\n")}`);
      }
    }
    record("DoD-3 熔断后 UI 横幅可见", banner !== null);
    const beforeRetry = (await snapshot(app)).daemonSpawnCount;
    await app.evaluate(() => globalThis.__aetherShell?.retryDaemon?.());
    const retried = await waitFor(
      async () => {
        const next = await snapshot(app);
        return next.daemonSpawnCount > beforeRetry ? next : null;
      },
      10_000,
      "手动重试拉起新 daemon",
    ).catch(() => null);
    record(
      "DoD-3 熔断后支持手动重试",
      retried !== null,
      retried ? `spawns=${retried.daemonSpawnCount}` : "",
    );
  } finally {
    if (launched.has(app)) {
      await closeApp(app);
    }
  }
}

async function scenarioShutdownStall(kind) {
  const ws = workspace(`stall-${kind}`);
  const { app, state } = await launch(
    { ...realEnv(ws), AETHER_TEST_SHUTDOWN_STALL: kind },
    `stall-${kind}`,
  );
  try {
    const started = Date.now();
    await Promise.race([app.close(), sleep(15_000)]);
    const elapsed = Date.now() - started;
    launched.delete(app);
    await sleep(500);
    const gone = !pidAlive(state.daemonPid);
    record(
      `DoD-2 ${kind} 卡住 → main 9.5s 强杀 / T11 ≤10s`,
      elapsed <= 10_000 && gone,
      `elapsedMs=${elapsed} daemonGone=${gone}`,
    );
  } finally {
    if (launched.has(app)) {
      await closeApp(app);
    }
  }
}

async function scenarioStuckShutdownTreeKill() {
  const ws = workspace("stuck");
  const childPidFile = path.join(ws.root, "child.pid");
  const { app, state } = await launch(stuckEnv(ws, childPidFile), "stuck");
  try {
    const childPid = await waitFor(
      async () => {
        if (!existsSync(childPidFile)) return null;
        const value = Number.parseInt(readFileSync(childPidFile, "utf8"), 10);
        return Number.isInteger(value) && value > 0 ? value : null;
      },
      10_000,
      "stuck daemon 子进程 pid",
    );
    const started = Date.now();
    await Promise.race([app.close(), sleep(15_000)]);
    const elapsed = Date.now() - started;
    launched.delete(app);
    await sleep(500);
    const childGone = !pidAlive(childPid);
    const daemonGone = !pidAlive(state.daemonPid);
    record(
      "DoD-2 daemon 卡死 → main ≤10s 整树回收",
      elapsed <= 10_000 && childGone && daemonGone,
      `elapsedMs=${elapsed} daemonGone=${daemonGone} childGone=${childGone}`,
    );
    killTree(state.pid);
    killTree(state.daemonPid);
    killTree(childPid);
  } finally {
    if (launched.has(app)) {
      await closeApp(app);
    }
  }
}

async function main() {
  if (!existsSync(daemonPath)) {
    throw new Error(`缺少 aetherd：${daemonPath}（先 cargo build -p aether-daemon）`);
  }
  if (!existsSync(path.join(appRoot, "dist", "index.html"))) {
    throw new Error("缺少 renderer 构建产物（先 pnpm build:desktop）");
  }
  try {
    const only = process.env.AETHER_M9_09_ONLY ?? "";
    if (only === "" || only === "timing") {
      console.log("[m9-09-e2e] scenario timing start");
      await scenarioUiTimingAndSessionCreate();
      console.log("[m9-09-e2e] scenario timing done");
    }
    if (only === "" || only === "restart") {
      console.log("[m9-09-e2e] scenario restart start");
      await scenarioRestartAfterKill();
      console.log("[m9-09-e2e] scenario restart done");
    }
    if (only === "" || only === "crash-loop") {
      console.log("[m9-09-e2e] scenario crash-loop start");
      await scenarioCrashLoopPauses();
      console.log("[m9-09-e2e] scenario crash-loop done");
    }
    if (only === "" || only === "checkpoint") {
      console.log("[m9-09-e2e] scenario checkpoint start");
      await scenarioShutdownStall("checkpoint");
      console.log("[m9-09-e2e] scenario checkpoint done");
    }
    if (only === "" || only === "write-queue") {
      console.log("[m9-09-e2e] scenario write-queue start");
      await scenarioShutdownStall("write_queue");
      console.log("[m9-09-e2e] scenario write-queue done");
    }
    if (only === "" || only === "stuck") {
      console.log("[m9-09-e2e] scenario stuck start");
      await scenarioStuckShutdownTreeKill();
      console.log("[m9-09-e2e] scenario stuck done");
    }
  } finally {
    for (const app of [...launched]) {
      killTree(app.process()?.pid);
      if (launched.has(app)) {
      await closeApp(app);
    }
    }
    for (const ws of workspaces) {
      safeRemove(ws.root);
    }
  }
  const failed = results.filter((item) => !item.ok);
  if (failed.length > 0) {
    console.error(`[m9-09-e2e] FAIL（${failed.length} 项）`);
    process.exit(1);
  }
  console.log(`[m9-09-e2e] PASS ${results.length} 项`);
  console.log("AETHER_M9_09_E2E PASS");
}

main().catch((error) => {
  console.error(`[m9-09-e2e] ERROR ${error.stack ?? error.message}`);
  for (const app of [...launched]) {
    killTree(app.process()?.pid);
  }
  process.exit(1);
});

