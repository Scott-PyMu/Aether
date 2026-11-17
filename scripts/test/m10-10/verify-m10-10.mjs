#!/usr/bin/env node
/** M10-10 验证入口：快速启动/并行检查/健康契约/诊断物理库/备份恢复预算/UI。 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let failures = 0;
const check = (label, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? `：${detail}` : ""}`);
  if (!ok) failures += 1;
};
const read = (relative) => readFileSync(path.join(root, relative), "utf8");
const json = (relative) => JSON.parse(read(relative));
const exists = (relative) => existsSync(path.join(root, relative));

const metrics = json("docs/M10-10-metrics.json");
check("证据存在 docs/M10-10-证据.md", exists("docs/M10-10-证据.md"));
check(
  "恢复 kill 故障注入测试存在",
  exists("crates/aether-store/tests/m10_10_restore_kill.rs"),
);
check(
  "quick_check 波动诊断测试存在",
  exists("crates/aether-app/tests/m10_10_check_diag.rs"),
);
check("周期完整检查测试存在", exists("crates/aether-app/tests/m10_10_periodic.rs"));
check("clean_start_count 规则测试存在", exists("crates/aether-app/tests/m10_10_clean_start.rs"));
check("页级损坏故障注入测试存在", exists("crates/aether-app/tests/m10_10_fault.rs"));
check("并发矩阵测试存在", exists("crates/aether-app/tests/m10_10_concurrency.rs"));
check("UI 校验/维护态测试存在", exists("apps/desktop/src/m10_10_storage_check.test.tsx"));
check("metrics task=M10-10", metrics.task === "M10-10");
check("core initialize ≤1.5s", metrics.core.initializeMs <= 1500, `${metrics.core.initializeMs}ms`);
check("core storageCheck ≤30s", metrics.core.storageCheckMs <= 30000, `${metrics.core.storageCheckMs}ms`);
check(
  "strict sidecar <1.5s",
  metrics.core.secondStorageCheckMs <= 1500,
  `${metrics.core.secondStorageCheckMs}ms`,
);
check("Electron 3 跑 window P50 ≤1.5s", metrics.electron.windowShownMs.p50 <= 1500);
check("Electron 3 跑 ui P50 ≤1.5s", metrics.electron.uiReadyMs.p50 <= 1500);
check(
  "Electron mainInteractive P50 ≤1.5s",
  (metrics.electron.mainInteractiveMs?.p50 ?? Number.POSITIVE_INFINITY) <= 1500,
);
check(
  "Electron 干净启动 writesEnabled P50 ≤1.5s",
  (metrics.electron.writesEnabledMs?.p50 ?? Number.POSITIVE_INFINITY) <= 1500,
);
check("Electron profile=release", metrics.electron.profile === "release");
check(
  "设备自适应并发 2..8",
  (metrics.device?.concurrency ?? 0) >= 2 && (metrics.device?.concurrency ?? 0) <= 8,
  `${metrics.device?.class}/${metrics.device?.concurrency}`,
);
check(
  "并发矩阵 2/4/8 齐全且每档 8 片",
  (metrics.concurrencyMatrix?.levels ?? []).length === 3 &&
    (metrics.concurrencyMatrix?.levels ?? []).every(
      (level) => level.concurrency !== undefined && level.perShardMs.length === 8,
    ),
);
check(
  "并发矩阵失败数为 0",
  (metrics.concurrencyMatrix?.levels ?? []).every((level) => level.failureCount === 0),
);
check("备份预算 ≤600s", metrics.backupRestore.backup_ms <= 600000);
check("恢复 staging ≤900s", metrics.backupRestore.stage_restore_ms <= 900000);
check("备份 manifest 10 文件", metrics.backupRestore.manifest_files === 10);

const storeWrite = read("crates/aether-store/src/write_queue.rs");
const storeOpen = read("crates/aether-store/src/store.rs");
const layout = read("crates/aether-store/src/layout.rs");
const health = read("crates/aether-app/src/core_health.rs");
const storageCheck = read("crates/aether-app/src/storage_check.rs");
const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
const healthMonitor = read("apps/desktop/src/HealthMonitor.tsx");
const fatal = read("apps/desktop/electron/fatal/fatal.js");
check("Store::open_deferred_check", storeOpen.includes("open_deferred_check"));
check("open_generation_deferred", storeWrite.includes("open_generation_deferred"));
check("WriteQueue pause/resume", storeWrite.includes("pause_writes") && storeWrite.includes("resume_writes"));
check("严格 sidecar", layout.includes("clean_shutdown_is_strict"));
check(
  "严格 sidecar 不匹配走全量（ADR-039，非 ADR-040）",
  layout.includes('full_startup_plan(layout, "fingerprint_mismatch")') &&
    !layout.includes("dirty_shards"),
);
check(
  "clean_start_count 仅在严格启动 +1",
  storeWrite.includes("let strict_start =") && storeWrite.includes("if strict_start"),
);
check(
  "设备自适应并发",
  storageCheck.includes("startup_check_profile") &&
    storageCheck.includes("StorageDeviceClass") &&
    storageCheck.includes("startup_check_concurrency"),
);
check(
  "并行 quick_check + 诊断",
  health.includes("quick_check_path") &&
    health.includes("record_check") &&
    storageCheck.includes("StorageCheckDiagnostics"),
);
check("动态 storage_check", health.includes("StorageCheckCell") && health.includes("with_storage_check_cell"));
check("动态 maintenance", health.includes("with_maintenance_cell"));
check("诊断物理 generation", diagnostics.includes("copy_generation_files") && diagnostics.includes("generation/aether.db"));
check("UI 校验进度（S-00）", healthMonitor.includes('data-testid="storage-check-progress"'));
check("UI 维护态（S-06）", healthMonitor.includes('data-testid="storage-maintenance"'));
check("UI 主库失败文案", healthMonitor.includes("storage_check_main_failed"));
check("S-12 incomplete_generation data-status", fatal.includes('dataset.status = "incomplete_generation"'));
check(
  "壳层 uiReady 兜底",
  read("apps/desktop/electron/app.ts").includes('source: "shell"'),
);
check(
  "Electron writesEnabledAt/storageCheckCompletedAt",
  read("apps/desktop/electron/app.ts").includes("writesEnabledAt") &&
    read("apps/desktop/electron/app.ts").includes("storageCheckCompletedAt"),
);

const run = (label, command, args, extraEnv = {}) => {
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: "inherit",
    shell: process.platform === "win32" && command.toLowerCase().endsWith(".cmd"),
    env: {
      ...process.env,
      CARGO_BUILD_JOBS: "1",
      CARGO_INCREMENTAL: "0",
      ...extraEnv,
    },
  });
  check(label, result.status === 0);
};
const cargo = process.env.CARGO ?? "cargo";
run("cargo m10_09_sharding", cargo, [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m10_09_sharding",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("cargo m10_10_restore_kill", cargo, [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m10_10_restore_kill",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("cargo m10_10_periodic", cargo, [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m10_10_periodic",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("cargo m10_10_clean_start", cargo, [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m10_10_clean_start",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("cargo m10_10_fault", cargo, [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m10_10_fault",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("cargo m5_03_encrypted_backup", cargo, [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m5_03_encrypted_backup",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);

const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
run(
  "desktop M10-10 UI tests",
  pnpm,
  ["--filter", "@aether/desktop", "test", "m10_10_storage_check.test.tsx"],
  { CARGO_BUILD_JOBS: "1" },
);

const dataDir = process.env.AETHER_M10_10_DATA_DIR;
if (dataDir && existsSync(dataDir)) {
  run(
    "cargo m10_10_startup（5.22GiB）",
    cargo,
    [
      "test",
      "-p",
      "aether-app",
      "--test",
      "m10_10_startup",
      "-j",
      "1",
      "--",
      "--ignored",
      "--nocapture",
      "--test-threads=1",
    ],
    { AETHER_M10_10_DATA_DIR: dataDir },
  );
} else {
  console.log("NOTE  未设置 AETHER_M10_10_DATA_DIR，跳过 5.22GiB core 启动复测");
}

if (dataDir && existsSync(dataDir) && process.env.AETHER_M10_10_RUN_PERF === "1") {
  run(
    "cargo m10_10_concurrency（2/4/8）",
    cargo,
    [
      "test",
      "-p",
      "aether-app",
      "--test",
      "m10_10_concurrency",
      "-j",
      "1",
      "--",
      "--ignored",
      "--nocapture",
      "--test-threads=1",
    ],
    {
      AETHER_M10_10_DATA_DIR: dataDir,
      AETHER_M10_10_CONCURRENCY: process.env.AETHER_M10_10_CONCURRENCY ?? "2,4,8",
    },
  );
} else {
  console.log("NOTE  未设置 AETHER_M10_10_RUN_PERF=1，跳过并发矩阵长跑（见 pnpm m10-10:concurrency）");
}

if (failures > 0) {
  console.error(`[m10-10] FAIL（${failures} 项）`);
  process.exit(1);
}
console.log("AETHER_M10_10 PASS");
