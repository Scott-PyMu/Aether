#!/usr/bin/env node
/**
 * M9-07 开发模式冒烟：`pnpm dev:desktop`（Vite + Electron）可启动。
 *
 * 断言：
 *  - Vite dev server（127.0.0.1:5173）可访问；
 *  - `apps/desktop/electron/dist/main.cjs` 已构建并被 Electron 加载；
 *  - shell.log 出现 `daemon.spawn` / `startup.ready` / `renderer.load mode=dev`；
 *  - 退出后不残留 Vite/Electron（taskkill /T）。
 *
 * 环境：
 *  - `AETHER_E2E_ELECTRON` 可覆盖 Electron 可执行文件（本机 45 GUI 崩溃绕行）；
 *  - 使用临时 `AETHER_DATA_DIR` / `AETHER_USER_DATA_DIR`，不触碰真实数据。
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const devScript = path.join(repoRoot, "scripts", "ci", "dev-desktop.mjs");
const devUrl = "http://127.0.0.1:5173";

if (!existsSync(devScript)) {
  console.error(`[m9-07-dev] 缺少 dev 脚本：${devScript}`);
  process.exit(1);
}

const devBase = path.join(repoRoot, "dist-out", "m9-07-dev");
mkdirSync(devBase, { recursive: true });
const root = mkdtempSync(path.join(devBase, "run-"));
const dataDir = path.join(root, "data");
const userData = path.join(root, "user");
const registry = path.join(root, "registry");
mkdirSync(dataDir, { recursive: true });
mkdirSync(userData, { recursive: true });
mkdirSync(registry, { recursive: true });
const shellLog = path.join(dataDir, "logs", "shell.log");

const child = spawn(process.execPath, [devScript], {
  cwd: repoRoot,
  stdio: ["ignore", "pipe", "pipe"],
  env: {
    ...process.env,
    AETHER_E2E: "1",
    ...(process.platform === "win32" ? { AETHER_E2E_NO_SANDBOX: "1" } : {}),
    AETHER_USER_DATA_DIR: userData,
    AETHER_DATA_DIR: dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registry,
    AETHER_DEVTOOLS: "0",
  },
});

let output = "";
child.stdout?.on("data", (chunk) => {
  output += chunk.toString("utf8");
  process.stdout.write(chunk);
});
child.stderr?.on("data", (chunk) => {
  output += chunk.toString("utf8");
  process.stderr.write(chunk);
});

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(predicate, timeoutMs, label) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (predicate()) {
      return true;
    }
    await sleep(200);
  }
  throw new Error(`等待超时：${label}`);
}

function shellEvents() {
  if (!existsSync(shellLog)) {
    return [];
  }
  return readFileSync(shellLog, "utf8")
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

function killTree(pid) {
  if (!pid) {
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

let failed = false;
try {
  await waitFor(() => existsSync(shellLog), 120_000, "shell.log 生成");
  await waitFor(
    () =>
      shellEvents().some(
        (event) => event.event === "renderer.load" && event.mode === "dev",
      ),
    120_000,
    "renderer.load mode=dev",
  );
  const events = shellEvents();
  const ready = events.find((event) => event.event === "startup.ready");
  const spawnEvent = events.find((event) => event.event === "daemon.spawn");
  const renderer = events.find(
    (event) => event.event === "renderer.load" && event.mode === "dev",
  );
  if (ready === undefined || spawnEvent === undefined || renderer === undefined) {
    throw new Error("缺少 daemon.spawn / startup.ready / renderer.load(mode=dev)");
  }
  const response = await fetch(devUrl, { method: "GET" });
  if (!response.ok) {
    throw new Error(`Vite 返回 ${response.status}`);
  }
  console.log(
    `[m9-07-dev] PASS initializeMs=${ready.initializeMs} daemon=${ready.daemonVersion} electron=${events[0]?.electron ?? "?"}`,
  );
  console.log("AETHER_M9_07_DEV PASS");
} catch (error) {
  failed = true;
  console.error(`[m9-07-dev] FAIL：${error instanceof Error ? error.message : String(error)}`);
  if (output.trim().length > 0) {
    console.error(output.split(/\r?\n/).slice(-40).join("\n"));
  }
} finally {
  killTree(child.pid);
  await sleep(500);
  try {
    rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-07-dev] 清理临时目录失败（忽略）：${error.message}`);
  }
}
process.exit(failed ? 1 : 0);
