#!/usr/bin/env node
/**
 * M10-10 5GB 冷启动端到端计时（M10-06 补充口径）。
 *
 * 用真实 `aetherd` + Electron 主进程/渲染器打开 5GB 数据集，记录：
 *   - `windowShownAt - startedAt`
 *   - `uiReadyAt - startedAt`（首个 health 成功）
 * 3 次运行，每次 fresh userData/registry；data dir 复用同一数据集。
 *
 * 运行前提：`pnpm build:desktop`、`cargo build -p aether-daemon`。
 *
 * 用法：
 *   node scripts/test/m10-10/startup-e2e.mjs --runs 3
 *   node scripts/test/m10-10/startup-e2e.mjs --dataset dist-out/m10-10/dataset --runs 3
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveFixedElectron } from "../m10-06/electron-acl-fix.mjs";
import { ensureDir, readJson, repoRootFrom, summarize, writeJson } from "../m10-08/lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const desktopRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(desktopRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");

function parseArgs(argv) {
  const options = {
    dataset:
      process.env.AETHER_M10_10_DATA_DIR ??
      path.join(os.tmpdir(), "opencode", "m10-09", "generation"),
    out: path.join(repoRoot, "dist-out", "m10-10", "results"),
    profile: process.env.AETHER_PROFILE ?? "debug",
    runs: 3,
    warmup: 1,
    invalidateSidecar: false,
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
      case "--profile":
        options.profile = next();
        break;
      case "--release":
        options.profile = "release";
        break;
      case "--runs":
        options.runs = Number(next());
        break;
      case "--warmup":
        options.warmup = Number(next());
        break;
      case "--invalidate-sidecar":
        options.invalidateSidecar = true;
        break;
      case "--timeout-ms":
        options.timeoutMs = Number(next());
        break;
      case "--help":
      case "-h":
        console.log(
          "用法：node scripts/test/m10-10/startup-e2e.mjs [--dataset DIR] [--runs N] [--warmup N] [--profile debug|release] [--release] [--invalidate-sidecar] [--timeout-ms N]",
        );
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!Number.isInteger(options.runs) || options.runs < 1) throw new Error("--runs 必须 ≥1");
  if (!Number.isInteger(options.warmup) || options.warmup < 0) {
    throw new Error("--warmup 必须 ≥0");
  }
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
  const currentPath = path.join(datasetDir, "CURRENT");
  if (!existsSync(currentPath)) {
    throw new Error(`generation CURRENT 不存在：${datasetDir}（M10-10 需要 M10-09 generation）`);
  }
  const manifest = readJson(manifestPath);
  const current = readJson(currentPath);
  return { manifest, rows: Number(manifest.rows), generation: current.generation };
}

async function runOnce(options, runIndex) {
  const electronPath = resolveFixedElectron(repoRoot);
  const daemonPath = path.join(
    repoRoot,
    "target",
    options.profile,
    process.platform === "win32" ? "aetherd.exe" : "aetherd",
  );
  if (!existsSync(electronPath)) throw new Error(`Electron 不存在：${electronPath}`);
  if (!existsSync(daemonPath)) {
    throw new Error(
      `aetherd 不存在：${daemonPath}（先 cargo build -p aether-daemon --profile ${options.profile}）`,
    );
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
  console.log(
    `[m10-10-startup] ${runIndex === 0 ? "warmup" : `run ${runIndex}`}: 真实 daemon + 5GB 数据集（${options.dataset}）`,
  );
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
        // 等首屏 + 写门开放/检查完成，保证 writesEnabledAt/storageCheckCompletedAt
        // 不是“尚未观测到”的 null（干净启动应 ≤1.5s；异常启动 = 检查完成）。
        return snapshot?.uiReadyAt && snapshot?.writesEnabledAt ? snapshot : null;
      },
      options.timeoutMs,
      "uiReadyAt + writesEnabledAt（首个 health/检查完成）",
    );
    const windowMs = timing.windowShownAt === null ? null : timing.windowShownAt - timing.startedAt;
    const uiMs = timing.uiReadyAt === null ? null : timing.uiReadyAt - timing.startedAt;
    const mainInteractiveMs =
      timing.mainInteractiveAt === null ? null : timing.mainInteractiveAt - timing.startedAt;
    const writesEnabledMs =
      timing.writesEnabledAt === null ? null : timing.writesEnabledAt - timing.startedAt;
    const storageCheckCompletedMs =
      timing.storageCheckCompletedAt === null
        ? null
        : timing.storageCheckCompletedAt - timing.startedAt;
    const result = {
      run: runIndex,
      at: new Date().toISOString(),
      profile: options.profile,
      windowShownMs: toNumberOrNull(windowMs),
      uiReadyMs: toNumberOrNull(uiMs),
      mainInteractiveMs: toNumberOrNull(mainInteractiveMs),
      writesEnabledMs: toNumberOrNull(writesEnabledMs),
      storageCheckCompletedMs: toNumberOrNull(storageCheckCompletedMs),
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
      `TIMING ${runIndex === 0 ? "warmup" : `run=${runIndex}`} windowShownMs=${result.windowShownMs} uiReadyMs=${result.uiReadyMs} ` +
        `mainInteractiveMs=${result.mainInteractiveMs} writesEnabledMs=${result.writesEnabledMs} ` +
        `storageCheckCompletedMs=${result.storageCheckCompletedMs} initializeMs=${result.initializeMs} ` +
        `daemonEverReady=${result.daemonEverReady}`,
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
    if (!process.env.AETHER_M10_10_KEEP_STARTUP_ARTIFACTS) {
      rmSync(workspace, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    }
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  ensureDir(options.out);
  const { manifest, rows, generation } = dataSetSummary(options.dataset);
  if (options.invalidateSidecar) {
    const sidecar = path.join(
      options.dataset,
      "generations",
      generation,
      "clean_shutdown.json",
    );
    rmSync(sidecar, { force: true });
    console.log(`[m10-10-startup] 已失效 sidecar：${sidecar}（异常启动路径）`);
  }
  console.log(
    `[m10-10-startup] 数据集：${rows.toLocaleString()} 行 / sha256=${String(manifest.sha256).slice(0, 16)}… / ` +
      `machine=${manifest.machine?.diskBusType ?? "unknown"}`,
  );
  const runs = [];
  let warmup = null;
  if (options.warmup > 0) {
    warmup = await runOnce(options, 0);
  }
  for (let index = 1; index <= options.runs; index += 1) {
    runs.push(await runOnce(options, index));
  }
  const windowSamples = runs.map((entry) => entry.windowShownMs);
  const uiSamples = runs.map((entry) => entry.uiReadyMs);
  const mainInteractiveSamples = runs.map((entry) => entry.mainInteractiveMs);
  const writesEnabledSamples = runs.map((entry) => entry.writesEnabledMs);
  const storageCheckSamples = runs.map((entry) => entry.storageCheckCompletedMs);
  const budgetMs = 1500;
  const result = {
    task: "M10-10",
    adr: "ADR-039 v1.0 D-039-3",
    profile: options.profile,
    dataset: {
      dir: safeRel(options.dataset),
      generation,
      rows,
      sha256: manifest.sha256,
      bytes: manifest.bytes,
    },
    machine: manifest.machine,
    runs,
    warmup,
    windowShownMs: summarize(windowSamples),
    uiReadyMs: summarize(uiSamples),
    mainInteractiveMs: summarize(mainInteractiveSamples),
    writesEnabledMs: summarize(writesEnabledSamples),
    storageCheckCompletedMs: summarize(storageCheckSamples),
    budgetMs,
    exceedsBudget: runs.some(
      (entry) =>
        (entry.windowShownMs ?? 0) > budgetMs || (entry.uiReadyMs ?? 0) > budgetMs,
    ),
  };
  const outPath = path.join(options.out, "startup-e2e.json");
  writeJson(outPath, result);
  console.log(
    `[m10-10-startup] OK：windowP50=${result.windowShownMs.p50}ms / uiP50=${result.uiReadyMs.p50}ms / ` +
      `budget=${budgetMs}ms / exceeds=${result.exceedsBudget}`,
  );
  console.log(JSON.stringify({ startup: outPath, ...result }, null, 2));
}

main().catch((error) => {
  console.error(`[m10-10-startup] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
