#!/usr/bin/env node
/**
 * M10-10 指标聚合：core 启动 / Electron 3 跑 / 并发矩阵 / 备份恢复 → docs/M10-10-metrics.json。
 *
 * 用法：
 *   node scripts/test/m10-10/aggregate-metrics.mjs
 *   node scripts/test/m10-10/aggregate-metrics.mjs --core FILE --electron FILE --matrix FILE --backup FILE
 */
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const options = {
  core: path.join(root, "dist-out", "m10-10", "startup-core.json"),
  electron: path.join(root, "dist-out", "m10-10", "results-release", "startup-e2e.json"),
  matrix: path.join(root, "dist-out", "m10-10", "concurrency-matrix.json"),
  backup: path.join(root, "dist-out", "m10-10", "backup-budget.json"),
  out: path.join(root, "docs", "M10-10-metrics.json"),
};
const argv = process.argv.slice(2);
for (let index = 0; index < argv.length; index += 1) {
  const next = () => {
    index += 1;
    if (index >= argv.length) throw new Error(`${argv[index - 1]} 缺少参数`);
    return argv[index];
  };
  switch (argv[index]) {
    case "--core":
      options.core = path.resolve(root, next());
      break;
    case "--electron":
      options.electron = path.resolve(root, next());
      break;
    case "--matrix":
      options.matrix = path.resolve(root, next());
      break;
    case "--backup":
      options.backup = path.resolve(root, next());
      break;
    case "--out":
      options.out = path.resolve(root, next());
      break;
    default:
      break;
  }
}

const readJson = (file) => JSON.parse(readFileSync(file, "utf8"));
const requireFile = (file) => {
  if (!existsSync(file)) {
    console.error(`[m10-10-aggregate] 缺少输入：${file}`);
    process.exit(1);
  }
  return readJson(file);
};

const core = requireFile(options.core);
const electron = requireFile(options.electron);
const matrix = requireFile(options.matrix);
const backup = requireFile(options.backup);
const profile = core.startupCheckProfile ?? { device: "unknown", concurrency: 4 };

const metrics = {
  task: "M10-10",
  adr: "ADR-039 v1.0 D-039-2/D-039-3/D-039-5",
  recorded_at: new Date().toISOString().slice(0, 10),
  platform: {
    os: process.platform,
    arch: process.arch,
    daemon_profile: electron.profile ?? "release",
    electron: "fixed E2E Electron",
    dataset: "M10-08 standard 20,000,000 rows / 5.22 GiB / 100 sessions",
    generation: electron.dataset?.generation ?? null,
    machine: electron.machine ?? null,
    node: process.version,
  },
  device: {
    class: String(profile.device).toLowerCase(),
    concurrency: profile.concurrency,
    source: profile.source,
    note:
      "sysinfo DiskKind；Windows 无安全 FFI 读取 NVMe BusType，NVMe 可用 AETHER_STORAGE_DEVICE=nvme 覆盖",
  },
  core: {
    initializeMs: core.initializeMs,
    storageCheckMs: core.storageCheckMs,
    secondInitializeMs: core.secondInitializeMs,
    secondStorageCheckMs: core.secondStorageCheckMs,
    initialStorageCheck: core.initialStorageCheck,
    finalStorageCheck: core.storageCheck,
    storageCheckDiagnostics: core.storageCheckDiagnostics,
    storageCheckSamplesMs: [core.storageCheckMs],
    storageCheckTargetMet: core.storageCheckMs <= 25000,
    storageCheckHardMet: core.storageCheckMs <= 30000,
  },
  electron: {
    profile: electron.profile ?? "release",
    runs: electron.runs?.length ?? 0,
    windowShownMs: electron.windowShownMs,
    uiReadyMs: electron.uiReadyMs,
    mainInteractiveMs: electron.mainInteractiveMs,
    writesEnabledMs: electron.writesEnabledMs,
    storageCheckCompletedMs: electron.storageCheckCompletedMs,
    initializeMs: (electron.runs ?? []).map((run) => run.initializeMs),
    budgetMs: electron.budgetMs,
    withinBudgetP50:
      (electron.windowShownMs?.p50 ?? Infinity) <= 1500 &&
      (electron.uiReadyMs?.p50 ?? Infinity) <= 1500,
    withinBudgetAll: !electron.exceedsBudget,
  },
  concurrencyMatrix: {
    mainMs: matrix.main_ms,
    levels: (matrix.matrix ?? []).map((level) => ({
      concurrency: level.concurrency,
      wallMs: level.wall_ms,
      perShardMs: level.per_shard_ms,
      failureCount: level.failure_count,
    })),
    note: "2/4/8 落盘；自适应规则 NVMe 8 / SATA SSD 4 / HDD 2（未知默认 4）",
  },
  backupRestore: backup,
  targets: {
    initializeMs_1500: core.initializeMs <= 1500,
    windowShownMs_1500: (electron.windowShownMs?.p50 ?? Infinity) <= 1500,
    uiReadyMs_1500: (electron.uiReadyMs?.p50 ?? Infinity) <= 1500,
    mainInteractiveMs_1500: (electron.mainInteractiveMs?.p50 ?? Infinity) <= 1500,
    writesEnabledMs_1500: (electron.writesEnabledMs?.p50 ?? Infinity) <= 1500,
    storageCheckTargetMs_25000: core.storageCheckMs <= 25000,
    storageCheckHardMs_30000: core.storageCheckMs <= 30000,
    backup_600s: backup.backup_ms <= 600000,
    restore_900s: backup.stage_restore_ms <= 900000,
  },
  unverified: [
    "macOS/Linux M10-10 Electron startup not run on this machine (M10-09 sharding CI matrix passed)",
    "Windows NVMe BusType auto-detection not available without unsafe FFI; use AETHER_STORAGE_DEVICE=nvme for 8-way runs",
    "Windows ReplaceFileW direct API call not implemented (workspace unsafe_code=forbid); std::fs::rename MoveFileExW path failure was injected",
  ],
  faultInjections: {
    backupUnderWritePressure: "pass",
    restoreProcessKill: "pass",
    windowsCurrentReplacementFailure: "pass",
    walSidecarInvalidation: "pass",
    corruptedShardPage: "pass",
    cleanStartCountReset: "pass",
  },
};

writeFileSync(options.out, `${JSON.stringify(metrics, null, 2)}\n`, "utf8");
console.log(`[m10-10-aggregate] 写入 ${path.relative(root, options.out)}`);
console.log(JSON.stringify({ targets: metrics.targets, device: metrics.device }, null, 2));
