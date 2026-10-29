#!/usr/bin/env node
/**
 * M9-09 DoD-4 E2E：Electron `kill -9` → daemon stdin EOF 优雅关闭 → 锁释放 →
 * 下次启动可打开同一数据目录（无跨壳/陈旧锁）。
 *
 * 运行前提：`pnpm build:desktop`、`cargo build -p aether-daemon`。
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
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
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

const results = [];
const launched = new Set();
let workspaceRoot = "";

function record(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const value = await fn();
    if (value) return value;
    await sleep(100);
  }
  throw new Error(`等待超时：${label}`);
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
  if (!pid || pid <= 0) return;
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

function makeWorkspace() {
  const base = path.join(repoRoot, "dist-out", "m9-09-crash");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, "run-"));
  workspaceRoot = root;
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const registry = path.join(root, "registry");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  mkdirSync(registry, { recursive: true });
  return { root, dataDir, userData, registry };
}

function envFor(ws) {
  return {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_USER_DATA_DIR: ws.userData,
    AETHER_DATA_DIR: ws.dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(ws.root, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: ws.registry,
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
    AETHERD_BIN: daemonPath,
  };
}

async function launch(ws) {
  const args = [...(noSandbox ? ["--no-sandbox"] : []), repoRoot];
  const app = await _electron.launch({
    executablePath: electronPath,
    args,
    cwd: repoRoot,
    env: envFor(ws),
    timeout: 30_000,
  });
  launched.add(app);
  return app;
}

async function snapshot(app) {
  return app.evaluate(() => globalThis.__aetherShell?.snapshot?.() ?? null);
}

async function waitReady(app) {
  return waitFor(
    async () => {
      const state = await snapshot(app);
      return state?.ready ? state : null;
    },
    30_000,
    "daemon ready",
  );
}

/** 直接启动真实 daemon，验证数据目录锁已释放（能完成 initialize）。 */
function initializeDaemon(dataDir, registry) {
  return new Promise((resolve, reject) => {
    const child = spawn(daemonPath, ["--stdio"], {
      env: {
        ...process.env,
        AETHER_DATA_DIR: dataDir,
        AETHER_RUNTIME_REGISTRY_DIR: registry,
        AETHER_DATA_LOCATION_FILE: path.join(path.dirname(dataDir), "probe-location.json"),
      },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    let buffer = "";
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("探针 daemon 10s 内无响应（锁未释放？）"));
    }, 10_000);
    child.stdout.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      const newline = buffer.indexOf("\n");
      if (newline < 0) return;
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      try {
        const frame = JSON.parse(line);
        if (frame.id === null) {
          clearTimeout(timer);
          child.kill();
          reject(new Error(`锁未释放：${JSON.stringify(frame)}`));
          return;
        }
        if (frame.id === 1 && frame.result) {
          clearTimeout(timer);
          child.stdin.end();
          child.kill();
          resolve(frame.result);
        }
      } catch {
        // 非 JSONL（不应发生）；继续等。
      }
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.stdin.write(
      `${JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "1.0",
          appVersion: "0.1.0",
          client: "e2e-probe",
          clientCapabilities: ["events"],
          requiredCapabilities: ["events"],
        },
      })}\n`,
    );
  });
}

async function main() {
  if (!existsSync(daemonPath)) {
    throw new Error(`缺少 aetherd：${daemonPath}`);
  }
  if (!existsSync(path.join(appRoot, "dist", "index.html"))) {
    throw new Error("缺少 renderer 构建产物（先 pnpm build:desktop）");
  }
  const ws = makeWorkspace();
  let app = await launch(ws);
  let state;
  try {
    state = await waitReady(app);
    record("DoD-4 Electron 崩溃前 daemon ready", state.ready === true, `pid=${state.daemonPid}`);

    // Electron main 强杀（Windows 用 TerminateProcess；子 daemon 不随 /T 一起被杀）。
    process.kill(state.pid, "SIGKILL");
    launched.delete(app);

    const daemonExited = await waitFor(
      async () => !pidAlive(state.daemonPid),
      12_000,
      "daemon stdin EOF 优雅退出",
    ).then(() => true).catch(() => false);
    record(
      "DoD-4 Electron kill -9 → daemon stdin EOF 退出（≤10s）",
      daemonExited,
      `daemonPid=${state.daemonPid}`,
    );

    const init = await initializeDaemon(ws.dataDir, ws.registry);
    record(
      "DoD-4 崩溃后数据目录锁释放（探针 daemon 可握手）",
      init?.protocolVersion === "1.0",
      `protocol=${init?.protocolVersion}`,
    );

    // 下次启动：同一数据目录可正常打开（孤儿清理/锁释放）。
    app = await launch(ws);
    const restarted = await waitReady(app);
    record(
      "DoD-4 下次启动可打开同一数据目录",
      restarted.ready === true,
      `pid=${restarted.daemonPid}`,
    );
  } finally {
    if (app) {
      killTree(app.process()?.pid);
      try {
        await Promise.race([app.close(), sleep(8_000)]);
      } catch {
        // 已退出。
      }
      launched.delete(app);
    }
    for (const item of [...launched]) {
      killTree(item.process()?.pid);
    }
    if (workspaceRoot) {
      try {
        rmSync(workspaceRoot, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      } catch {
        // 句柄滞后；忽略清理失败。
      }
    }
  }

  const failed = results.filter((item) => !item.ok);
  if (failed.length > 0) {
    console.error(`[m9-09-crash] FAIL（${failed.length} 项）`);
    process.exit(1);
  }
  console.log(`[m9-09-crash] PASS ${results.length} 项`);
  console.log("AETHER_M9_09_CRASH PASS");
}

main().catch((error) => {
  console.error(`[m9-09-crash] ERROR ${error.stack ?? error.message}`);
  if (workspaceRoot) {
    try {
      rmSync(workspaceRoot, { recursive: true, force: true, maxRetries: 3 });
    } catch {
      // 忽略。
    }
  }
  process.exit(1);
});

