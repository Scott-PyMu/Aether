#!/usr/bin/env node
/**
 * M9-09 DoD-1 真实后端会话创建计时（Mock 运行时）。
 *
 * 前置（本机验收）：
 *   $env:AETHER_BUNDLE_MOCK="1"
 *   node scripts/ci/build-runtime-bundles.mjs --out crates/aether-tauri/runtime-bundle
 *   cargo build -p aether-daemon          # build.rs 内嵌 mock 程序摘要
 *
 * 断言：UI 首个 health 成功后调用 `session_create(runtime_id="mock")`，
 * main 记录的 `sessionCreateMs` < 2000ms。
 */
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { spawnSync } from "node:child_process";

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
const registryDir =
  process.env.AETHER_M9_09_REAL_REGISTRY ??
  path.join(repoRoot, "crates", "aether-tauri", "runtime-bundle");
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

function fail(message) {
  console.error(`[m9-09-real-session] FAIL ${message}`);
  process.exit(1);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let last;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      last = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${last ? `（${last.message}）` : ""}`);
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

async function main() {
  if (!existsSync(daemonPath)) fail(`缺少 aetherd：${daemonPath}`);
  if (!existsSync(path.join(appRoot, "dist", "index.html"))) {
    fail("缺少 renderer 构建产物（先 pnpm build:desktop）");
  }
  if (!existsSync(path.join(registryDir, "aether-mock-adapter.exe")) &&
      !existsSync(path.join(registryDir, "aether-mock-adapter"))) {
    fail(`缺少 Mock 运行时 bundle：${registryDir}（先 AETHER_BUNDLE_MOCK=1 构建并重建 aetherd）`);
  }

  const base = path.join(repoRoot, "dist-out", "m9-09-real-session");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, "run-"));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });

  const app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
    cwd: repoRoot,
    env: {
      ...process.env,
      AETHER_E2E: "1",
      AETHER_USER_DATA_DIR: userData,
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
      AETHER_RUNTIME_REGISTRY_DIR: registryDir,
      AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
      AETHERD_BIN: daemonPath,
      // 隔离 D5 台账（~/.aether/run）到工作区，避免宿主机临时目录/ACL 干扰。
      USERPROFILE: root,
      HOME: root,
    },
    timeout: 30_000,
  });

  try {
    await waitFor(
      async () => {
        const state = await app.evaluate(() => globalThis.__aetherShell?.snapshot?.() ?? null);
        return state?.ready ? state : null;
      },
      30_000,
      "daemon ready",
    );
    const page = await waitFor(
      async () => {
        const pages = app.windows().filter((item) => !item.isClosed());
        return pages.find((item) => item.url() === "aether://app/index.html") ?? null;
      },
      15_000,
      "主窗口",
    );
    const call = async (method, params) =>
      page.evaluate(
        ([name, value]) => window.aether.rpc(name, value),
        [method, params],
      );
    await waitFor(
      async () => {
        const envelope = await call("health", undefined);
        return envelope?.ok === true ? true : null;
      },
      20_000,
      "首个 health 成功",
    );
    const created = await call("session_create", {
      runtime_id: "mock",
      title: "M9-09 real session timing",
    });
    if (!created?.ok) {
      const logPath = path.join(dataDir, "logs", "aetherd.log");
      if (existsSync(logPath)) {
        const tail = readFileSync(logPath, "utf8").split(/\r?\n/).slice(-40).join("\n");
        console.error(`[m9-09-real-session] aetherd.log 尾部：\n${tail}`);
      }
      fail(`session_create 失败：${JSON.stringify(created?.error ?? created)}`);
    }
    const state = await waitFor(
      async () => {
        const next = await app.evaluate(() => globalThis.__aetherShell?.snapshot?.() ?? null);
        return next?.sessionCreateMs !== null ? next : null;
      },
      10_000,
      "session_create 计时",
    );
    if (!(state.sessionCreateMs < 2_000)) {
      fail(`session_create 超 2s：${state.sessionCreateMs}ms`);
    }
    console.log(
      `PASS  DoD-1 真实后端会话创建 <2s  sessionCreateMs=${state.sessionCreateMs} uiReadyAt=${state.uiReadyAt}`,
    );
    console.log("AETHER_M9_09_REAL_SESSION PASS");
  } finally {
    const pid = app.process()?.pid;
    try {
      await Promise.race([app.close(), sleep(8_000)]);
    } catch {
      // 已退出。
    }
    killTree(pid);
    try {
      rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch {
      // 句柄滞后；忽略。
    }
  }
}

main().catch((error) => {
  console.error(`[m9-09-real-session] ERROR ${error.stack ?? error.message}`);
  process.exit(1);
});
