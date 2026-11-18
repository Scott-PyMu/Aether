#!/usr/bin/env node
/**
 * M10-14 / ADR-040：真实 Electron + release `aetherd` 的 dirty 1/2/4/8 启动矩阵。
 *
 * 每个 dirty 档使用一份 5.22 GiB generation 副本；第 1 次运行是复制后首次（冷-ish），
 * 后续为热运行。每档默认 3 次。记录 shell `windowShownAt/uiReadyAt/writesEnabledAt/
 * startupCheckCompletedAt` 与 `health.storage_check` v2 字段。
 *
 * 用法：
 *   node scripts/test/m10-14/electron-matrix.mjs --dataset <generation> --runs 3
 */
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveFixedElectron } from "../m10-06/electron-acl-fix.mjs";
import { ensureDir, readJson, repoRootFrom, summarize, writeJson } from "../m10-08/lib.mjs";

const root = repoRootFrom(import.meta.url);
const desktopRoot = path.join(root, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(desktopRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");

function parseArgs(argv) {
  const options = {
    dataset:
      process.env.AETHER_M10_14_DATA_DIR ??
      path.join(process.env.TEMP ?? ".", "opencode", "m10-14-work", "base"),
    out: path.join(root, "dist-out", "m10-14", "electron"),
    profile: "release",
    runs: 3,
    dirty: [1, 2, 4, 8],
    timeoutMs: 240_000,
  };
  for (let index = 2; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--dataset":
        options.dataset = path.resolve(root, next());
        break;
      case "--out":
        options.out = path.resolve(root, next());
        break;
      case "--runs":
        options.runs = Number(next());
        break;
      case "--dirty":
        options.dirty = next()
          .split(",")
          .map((value) => Number(value.trim()))
          .filter((value) => Number.isInteger(value) && value >= 1 && value <= 8);
        break;
      case "--timeout-ms":
        options.timeoutMs = Number(next());
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!existsSync(path.join(options.dataset, "CURRENT"))) {
    throw new Error(`dataset 不是 generation（缺 CURRENT）：${options.dataset}`);
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
    execFileSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
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

function prepareDirtySidecar(dataset, dirtyCount) {
  const current = readJson(path.join(dataset, "CURRENT"));
  const sidecarPath = path.join(
    dataset,
    "generations",
    current.generation,
    "clean_shutdown.json",
  );
  if (!existsSync(sidecarPath)) {
    throw new Error(`缺少 clean_shutdown.json：先对 base generation 执行 clean shutdown`);
  }
  const sidecar = JSON.parse(readFileSync(sidecarPath, "utf8"));
  sidecar.format_version = 1;
  sidecar.background_check_completed = true;
  sidecar.last_full_check_at = Date.now();
  sidecar.clean_start_count = 0;
  for (let id = 0; id < dirtyCount; id += 1) {
    const entry = sidecar.shards.find((candidate) => candidate.shard_id === id);
    if (!entry) throw new Error(`sidecar 缺少分片 ${id}`);
    entry.header_change_counter = Number(entry.header_change_counter ?? 0) + 1;
  }
  writeFileSync(sidecarPath, `${JSON.stringify(sidecar, null, 2)}\n`, "utf8");
}

async function runOnce(options, dir, dirtyCount, runIndex) {
  prepareDirtySidecar(dir, dirtyCount);
  const electronPath = resolveFixedElectron(root);
  const daemonPath = path.join(
    root,
    "target",
    options.profile,
    process.platform === "win32" ? "aetherd.exe" : "aetherd",
  );
  if (!existsSync(electronPath)) throw new Error(`Electron 不存在：${electronPath}`);
  if (!existsSync(daemonPath)) {
    throw new Error(`aetherd 不存在：${daemonPath}（先 cargo build --release -p aether-daemon）`);
  }
  if (!existsSync(path.join(desktopRoot, "dist", "index.html"))) {
    throw new Error("renderer 构建产物不存在（先 pnpm build:desktop）");
  }
  const workspace = mkdtempSync(path.join(options.out, `electron-d${dirtyCount}-`));
  const userData = path.join(workspace, "user");
  const registry = path.join(workspace, "registry");
  for (const entry of [userData, registry]) mkdirSync(entry, { recursive: true });
  const noSandbox = process.env.AETHER_E2E_NO_SANDBOX === "1" || process.platform === "win32";
  const env = {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_USER_DATA_DIR: userData,
    AETHER_DATA_DIR: dir,
    AETHER_DATA_LOCATION_FILE: path.join(workspace, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registry,
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
    AETHERD_BIN: daemonPath,
  };
  console.log(`[m10-14-electron] dirty=${dirtyCount} run=${runIndex}（真实 daemon + Electron）`);
  const app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), root],
    cwd: root,
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
        return snapshot?.uiReadyAt && snapshot?.writesEnabledAt ? snapshot : null;
      },
      options.timeoutMs,
      "uiReadyAt + writesEnabledAt",
    );
    const healthEnvelope = await page.evaluate(async () => {
      const api = globalThis.window?.aether;
      return api && typeof api.rpc === "function" ? api.rpc("health") : null;
    });
    const health = healthEnvelope?.ok ? healthEnvelope.result : null;
    const check = health?.storage_check ?? null;
    if (!check) throw new Error("health.storage_check 缺失");
    if (check.contract_version !== 2) {
      throw new Error(`contract_version 期望 2，实际 ${check.contract_version}`);
    }
    if (check.state !== "ok" || check.write_gate !== "open") {
      throw new Error(`检查未完成：state=${check.state} write_gate=${check.write_gate}`);
    }
    const expectedPlanned = dirtyCount;
    const expectedSkipped = 8 - dirtyCount;
    const expectedFull = dirtyCount === 8;
    if (check.planned_shards !== expectedPlanned) {
      throw new Error(`planned_shards 期望 ${expectedPlanned}，实际 ${check.planned_shards}`);
    }
    if (check.skipped_shards !== expectedSkipped) {
      throw new Error(`skipped_shards 期望 ${expectedSkipped}，实际 ${check.skipped_shards}`);
    }
    if (check.full_coverage !== expectedFull) {
      throw new Error(`full_coverage 期望 ${expectedFull}，实际 ${check.full_coverage}`);
    }
    if (check.checked_shards !== dirtyCount) {
      throw new Error(`checked_shards 期望 ${dirtyCount}，实际 ${check.checked_shards}`);
    }
    const ms = (value) => (value === null || value === undefined ? null : value - timing.startedAt);
    const result = {
      dirty: dirtyCount,
      run: runIndex,
      coldFirst: runIndex === 1,
      windowShownMs: ms(timing.windowShownAt),
      uiReadyMs: ms(timing.uiReadyAt),
      mainInteractiveMs: ms(timing.mainInteractiveAt),
      writesEnabledMs: ms(timing.writesEnabledAt),
      startupCheckCompletedMs:
        timing.startupCheckCompletedAt === null || timing.startupCheckCompletedAt === undefined
          ? ms(timing.storageCheckCompletedAt)
          : ms(timing.startupCheckCompletedAt),
      fullCoverageCheckCompletedMs: ms(timing.fullCoverageCheckCompletedAt),
      storageCheckCompletedMs: ms(timing.storageCheckCompletedAt),
      health: {
        contract_version: check.contract_version,
        state: check.state,
        mode: check.mode,
        write_gate: check.write_gate,
        main_checked: check.main_checked,
        checked_shards: check.checked_shards,
        total_shards: check.total_shards,
        planned_shards: check.planned_shards,
        skipped_shards: check.skipped_shards,
        full_coverage: check.full_coverage,
      },
      initializeMs: timing.initializeMs,
      daemonEverReady: timing.daemonEverReady,
      fatal: timing.fatal,
    };
    console.log(
      `[m10-14-electron] dirty=${dirtyCount} run=${runIndex} ` +
        `window=${result.windowShownMs} ui=${result.uiReadyMs} ` +
        `writes=${result.writesEnabledMs} startupCheck=${result.startupCheckCompletedMs} ` +
        `planned=${check.planned_shards} skipped=${check.skipped_shards} full=${check.full_coverage}`,
    );
    return result;
  } finally {
    try {
      await Promise.race([app.close(), sleep(20_000)]);
    } catch {
      // 已退出/竞态。
    }
    if (pid && pidAlive(pid)) killTree(pid);
    await sleep(300);
    if (!process.env.AETHER_M10_14_KEEP_ELECTRON_ARTIFACTS) {
      rmSync(workspace, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    }
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  ensureDir(options.out);
  const workRoot = path.join(options.out, "work");
  ensureDir(workRoot);
  const results = [];
  for (const dirtyCount of options.dirty) {
    const cellDir = path.join(workRoot, `generation-d${dirtyCount}`);
    rmSync(cellDir, { recursive: true, force: true });
    console.log(`[m10-14-electron] 复制 generation → ${cellDir}`);
    cpSync(options.dataset, cellDir, { recursive: true });
    try {
      for (let run = 1; run <= options.runs; run += 1) {
        results.push(await runOnce(options, cellDir, dirtyCount, run));
      }
    } finally {
      if (!process.env.AETHER_M10_14_KEEP_ELECTRON_ARTIFACTS) {
        rmSync(cellDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      }
    }
  }
  const matrix = {};
  for (const dirtyCount of options.dirty) {
    const cells = results.filter((cell) => cell.dirty === dirtyCount);
    matrix[dirtyCount] = {
      dirty: dirtyCount,
      runs: cells,
      startupCheckCompletedMs: summarize(
        cells.map((cell) => cell.startupCheckCompletedMs).filter((value) => value !== null),
      ),
      writesEnabledMs: summarize(
        cells.map((cell) => cell.writesEnabledMs).filter((value) => value !== null),
      ),
      uiReadyMs: summarize(
        cells.map((cell) => cell.uiReadyMs).filter((value) => value !== null),
      ),
      coldFirst: cells.find((cell) => cell.coldFirst) ?? null,
    };
  }
  const report = {
    task: "M10-14",
    adr: "ADR-040 v1.0",
    recorded_at: new Date().toISOString(),
    profile: options.profile,
    dataset: options.dataset,
    runs_per_cell: options.runs,
    matrix,
  };
  const outPath = path.join(options.out, "electron-matrix.json");
  writeJson(outPath, report);
  console.log(`[m10-14-electron] OK：${outPath}`);
  for (const dirtyCount of options.dirty) {
    const entry = matrix[dirtyCount];
    console.log(
      `dirty=${dirtyCount} startupCheck P50=${entry.startupCheckCompletedMs.p50}ms ` +
        `P95=${entry.startupCheckCompletedMs.p95}ms max=${entry.startupCheckCompletedMs.max}ms`,
    );
  }
}

main().catch((error) => {
  console.error(`[m10-14-electron] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
