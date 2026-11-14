#!/usr/bin/env node
/**
 * M10-08 5GB 冷启动端到端计时（M10-06 补充口径）。
 *
 * 用真实 `aetherd` + Electron 主进程/渲染器打开 5GB 数据集，记录：
 *   - `windowShownAt - startedAt`
 *   - `uiReadyAt - startedAt`（首个 health 成功）
 * 3 次运行，每次 fresh userData/registry；data dir 复用同一数据集。
 *
 * 运行前提：`pnpm build:desktop`、`cargo build -p aether-daemon`。
 *
 * 用法：
 *   node scripts/test/m10-08/startup-e2e.mjs --runs 3
 *   node scripts/test/m10-08/startup-e2e.mjs --dataset dist-out/m10-08/dataset --runs 3
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveFixedElectron } from "../m10-06/electron-acl-fix.mjs";
import { THRESHOLDS, ensureDir, openDatabase, readJson, repoRootFrom, summarize, writeJson } from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const desktopRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(desktopRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");

function parseArgs(argv) {
  const options = {
    dataset: path.join(repoRoot, "dist-out", "m10-08", "dataset"),
    out: path.join(repoRoot, "dist-out", "m10-08", "results"),
    runs: 3,
    timeoutMs: 180_000,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--dataset":
        options.dataset = path.resolve(repoRoot, next());
        break;
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--runs":
        options.runs = Number(next());
        break;
      case "--timeout-ms":
        options.timeoutMs = Number(next());
        break;
      case "--help":
      case "-h":
        console.log("用法：node scripts/test/m10-08/startup-e2e.mjs [--dataset DIR] [--runs N] [--timeout-ms N]");
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!Number.isInteger(options.runs) || options.runs < 1) throw new Error("--runs 必须 ≥1");
  return options;
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
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], { stdio: "ignore", timeout: 10_000 });
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

function safeRel(filePath) {
  const relative = path.relative(repoRoot, filePath ?? "");
  if (!relative || relative.startsWith("..") || path.isAbsolute(relative)) {
    return `<external>/${path.basename(filePath ?? "unknown")}`;
  }
  return relative.replaceAll("\\", "/");
}

function toNumberOrNull(value) {
  return value === null || value === undefined ? null : Number(value);
}

function dataSetSummary(datasetDir) {
  const manifestPath = path.join(datasetDir, "dataset.json");
  if (!existsSync(manifestPath)) throw new Error(`数据集 manifest 不存在：${datasetDir}`);
  const manifest = readJson(manifestPath);
  const db = openDatabase(path.join(datasetDir, "aether.db"), { readOnly: true });
  const rows = Number(db.prepare("SELECT COUNT(*) AS count FROM events").get().count);
  db.close();
  return { manifest, rows };
}

async function runOnce(options, runIndex) {
  const electronPath = resolveFixedElectron(repoRoot);
  const daemonPath = path.join(
    repoRoot,
    "target",
    "debug",
    process.platform === "win32" ? "aetherd.exe" : "aetherd",
  );
  if (!existsSync(electronPath)) throw new Error(`Electron 不存在：${electronPath}`);
  if (!existsSync(daemonPath)) {
    throw new Error(`aetherd 不存在：${daemonPath}（先 cargo build -p aether-daemon）`);
  }
  if (!existsSync(path.join(desktopRoot, "dist", "index.html"))) {
    throw new Error("renderer 构建产物不存在（先 pnpm build:desktop）");
  }
  const workspace = mkdtempSync(path.join(options.out, "startup-"));
  const userData = path.join(workspace, "user");
  const registry = path.join(workspace, "registry");
  for (const dir of [userData, registry]) mkdirSync(dir, { recursive: true });
  const noSandbox = process.env.AETHER_E2E_NO_SANDBOX === "1" || process.platform === "win32";
  const env = {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_USER_DATA_DIR: userData,
    AETHER_DATA_DIR: options.dataset,
    AETHER_DATA_LOCATION_FILE: path.join(workspace, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registry,
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
    AETHERD_BIN: daemonPath,
  };
  console.log(`[m10-08-startup] run ${runIndex}: 真实 daemon + 5GB 数据集（${options.dataset}）`);
  const app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
    cwd: repoRoot,
    env,
    timeout: options.timeoutMs,
  });
  let pid;
  try {
    pid = app.process()?.pid;
    const page = await app.firstWindow();
    await waitFor(
      async () => page.locator('[data-testid="workbench"]').count(),
      options.timeoutMs,
      "主界面 workbench",
    );
    const timing = await waitFor(
      async () => {
        const snapshot = await app.evaluate(() => {
          const hook = globalThis.__aetherShell;
          return hook && typeof hook.snapshot === "function" ? hook.snapshot() : null;
        });
        return snapshot?.uiReadyAt ? snapshot : null;
      },
      options.timeoutMs,
      "uiReadyAt（首个 health 成功）",
    );
    const windowMs = timing.windowShownAt === null ? null : timing.windowShownAt - timing.startedAt;
    const uiMs = timing.uiReadyAt === null ? null : timing.uiReadyAt - timing.startedAt;
    const result = {
      run: runIndex,
      at: new Date().toISOString(),
      windowShownMs: toNumberOrNull(windowMs),
      uiReadyMs: toNumberOrNull(uiMs),
      initializeMs: timing.initializeMs,
      withinTarget: timing.withinTarget,
      daemonSpawnCount: timing.daemonSpawnCount,
      daemonEverReady: timing.daemonEverReady,
      fatal: timing.fatal,
      ready: timing.ready,
      phase: timing.phase,
      shellLog: safeRel(timing.shellLogPath),
      daemonLog: safeRel(timing.daemonLogPath),
    };
    console.log(
      `TIMING run=${runIndex} windowShownMs=${result.windowShownMs} uiReadyMs=${result.uiReadyMs} ` +
        `initializeMs=${result.initializeMs} daemonEverReady=${result.daemonEverReady}`,
    );
    return result;
  } finally {
    try {
      await Promise.race([app.close(), sleep(20_000)]);
    } catch {
      // 已退出/竞态。
    }
    const daemonPid = pid;
    if (daemonPid && pidAlive(daemonPid)) killTree(daemonPid);
    await sleep(500);
    if (!process.env.AETHER_M10_08_KEEP_STARTUP_ARTIFACTS) {
      rmSync(workspace, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    }
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  ensureDir(options.out);
  const { manifest, rows } = dataSetSummary(options.dataset);
  console.log(
    `[m10-08-startup] 数据集：${rows.toLocaleString()} 行 / sha256=${String(manifest.sha256).slice(0, 16)}… / ` +
      `machine=${manifest.machine?.diskBusType ?? "unknown"}`,
  );
  const runs = [];
  for (let index = 1; index <= options.runs; index += 1) {
    runs.push(await runOnce(options, index));
  }
  const windowSamples = runs.map((entry) => entry.windowShownMs);
  const uiSamples = runs.map((entry) => entry.uiReadyMs);
  const result = {
    task: "M10-08",
    adr: "ADR-038 + M10-06 启动体验补充口径",
    dataset: {
      dir: safeRel(options.dataset),
      rows,
      sha256: manifest.sha256,
      bytes: manifest.bytes,
    },
    machine: manifest.machine,
    runs,
    windowShownMs: summarize(windowSamples),
    uiReadyMs: summarize(uiSamples),
    budgetMs: THRESHOLDS.startupBudgetMs,
    exceedsBudget: runs.some(
      (entry) =>
        (entry.windowShownMs ?? 0) > THRESHOLDS.startupBudgetMs ||
        (entry.uiReadyMs ?? 0) > THRESHOLDS.startupBudgetMs,
    ),
  };
  const outPath = path.join(options.out, "startup-e2e.json");
  writeJson(outPath, result);
  console.log(
    `[m10-08-startup] OK：windowP50=${result.windowShownMs.p50}ms / uiP50=${result.uiReadyMs.p50}ms / ` +
      `budget=${THRESHOLDS.startupBudgetMs}ms / exceeds=${result.exceedsBudget}`,
  );
  console.log(JSON.stringify({ startup: outPath, ...result }, null, 2));
}

main().catch((error) => {
  console.error(`[m10-08-startup] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
