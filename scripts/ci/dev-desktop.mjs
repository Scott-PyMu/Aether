#!/usr/bin/env node
/**
 * 开发模式启动器（M9-07 DoD-1）：`pnpm dev:desktop`。
 *
 * 流程：
 *   1. 确保 `aetherd` 已构建（缺省 target/debug；`--rebuild-daemon` 强制重建）；
 *   2. 构建 Electron main/preload（`apps/desktop/electron/dist`）；
 *   3. 启动 Vite dev server（127.0.0.1:5173，strictPort）；
 *   4. 以 `AETHER_RENDERER_URL` 启动 Electron；退出时回收 Vite。
 *
 * 参数透传：`pnpm dev:desktop -- --rebuild-daemon`；其余参数交给 Electron。
 * 环境变量（AETHER_DATA_DIR / AETHER_RUNTIME_BUNDLE_DIR 等）原样继承。
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../test/lib/exec.mjs";
import { resolveElectronExecutable } from "./electron-bin.mjs";

const appRoot = path.join(repoRoot, "apps", "desktop");
const electronEntry = path.join(appRoot, "node_modules", "electron", "index.js");
const viteBin = path.join(appRoot, "node_modules", "vite", "bin", "vite.js");
const devUrl = "http://127.0.0.1:5173";
const passthrough = process.argv.slice(2);
const rebuildDaemon = passthrough.includes("--rebuild-daemon");
const electronArgs = passthrough.filter((arg) => arg !== "--rebuild-daemon");

function fail(message) {
  console.error(`[dev] ${message}`);
  process.exit(1);
}

if (!existsSync(electronEntry)) {
  fail(
    `未找到 Electron（${electronEntry}）。请先在仓库根执行 pnpm install --frozen-lockfile。`,
  );
}
if (!existsSync(viteBin)) {
  fail(`未找到 Vite（${viteBin}）。请先在仓库根执行 pnpm install --frozen-lockfile。`);
}

function runCommand(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? repoRoot,
    stdio: "inherit",
    env: { ...process.env, ...(options.env ?? {}) },
    shell: false,
  });
  if (result.error) {
    fail(`无法执行 ${command}：${result.error.message}`);
  }
  return result.status ?? 1;
}

// 1) daemon
const daemonName = process.platform === "win32" ? "aetherd.exe" : "aetherd";
const daemonPath = path.join(repoRoot, "target", "debug", daemonName);
if (rebuildDaemon || !existsSync(daemonPath)) {
  console.log(`[dev] 构建 aetherd（${daemonPath}）…`);
  const cargo = bin("cargo");
  const code = runCommand(cargo, ["build", "-p", "aether-daemon"], {
    env: {
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
      CARGO_INCREMENTAL: process.env.CARGO_INCREMENTAL ?? "0",
    },
  });
  if (code !== 0 || !existsSync(daemonPath)) {
    fail("aetherd 构建失败（cargo build -p aether-daemon）");
  }
}
console.log(`[dev] daemon：${daemonPath}`);

// 2) Electron 构建
console.log("[dev] 构建 Electron main/preload…");
const buildCode = runCommand(process.execPath, [path.join(appRoot, "electron", "build.mjs")], {
  cwd: appRoot,
});
if (buildCode !== 0) {
  fail("Electron main/preload 构建失败");
}

// 3) Vite
console.log(`[dev] 启动 Vite：${devUrl}`);
const vite = spawn(process.execPath, [viteBin, "--host", "127.0.0.1", "--port", "5173", "--strictPort"], {
  cwd: appRoot,
  stdio: ["ignore", "inherit", "inherit"],
  env: process.env,
});

async function waitForVite(timeoutMs = 30_000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      const response = await fetch(devUrl, { method: "GET" });
      if (response.ok) {
        return true;
      }
    } catch {
      // 尚未监听。
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  return false;
}

const ready = await waitForVite();
if (!ready) {
  vite.kill();
  fail(`Vite 在 ${devUrl} 未就绪（30s）`);
}

// 4) Electron
let electronPath;
try {
  electronPath = resolveElectronExecutable({ repoRoot });
} catch (error) {
  vite.kill();
  fail(`无法解析 Electron 可执行文件：${error.message}`);
}

console.log(`[dev] Electron：${electronPath}`);

const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" || process.env.AETHER_NO_SANDBOX === "1";
const launchArgs = [
  ...(noSandbox ? ["--no-sandbox"] : []),
  repoRoot,
  ...electronArgs,
];
console.log("[dev] 启动 Electron（Ctrl+C 结束）");
const child = spawn(electronPath, launchArgs, {
  cwd: repoRoot,
  stdio: "inherit",
  env: {
    ...process.env,
    AETHER_RENDERER_URL: devUrl,
    AETHER_DEVTOOLS: process.env.AETHER_DEVTOOLS ?? "1",
  },
});

let shuttingDown = false;
function shutdown(code) {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  if (!vite.killed) {
    vite.kill();
  }
  if (!child.killed && child.exitCode === null) {
    child.kill();
  }
  process.exit(code);
}

child.on("exit", (code, signal) => {
  console.error(`[dev] Electron 已退出（code=${code ?? "null"}, signal=${signal ?? "null"}）`);
  shutdown(code ?? (signal ? 130 : 1));
});
vite.on("exit", (code) => {
  if (!shuttingDown) {
    console.error(`[dev] Vite 已退出（code=${code}）`);
    shutdown(code ?? 1);
  }
});
process.on("SIGINT", () => shutdown(130));
process.on("SIGTERM", () => shutdown(130));
