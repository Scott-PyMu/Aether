#!/usr/bin/env node
/** M10-10 验证入口：快速启动/并行检查/健康契约/诊断物理库/备份恢复预算。 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let failures = 0;
const check = (label, ok) => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) failures += 1;
};
const read = (relative) => readFileSync(path.join(root, relative), "utf8");
const json = (relative) => JSON.parse(read(relative));

const metrics = json("docs/M10-10-metrics.json");
check("证据存在 docs/M10-10-证据.md", existsSync(path.join(root, "docs/M10-10-证据.md")));
check(
  "恢复 kill 故障注入测试存在",
  existsSync(path.join(root, "crates/aether-store/tests/m10_10_restore_kill.rs")),
);
check(
  "quick_check 波动诊断测试存在",
  existsSync(path.join(root, "crates/aether-app/tests/m10_10_check_diag.rs")),
);
check("metrics task=M10-10", metrics.task === "M10-10");
check("core initialize ≤1.5s", metrics.core.initializeMs <= 1500);
check("core storageCheck ≤30s", metrics.core.storageCheckMs <= 30000);
check("strict sidecar <1.5s", metrics.core.secondStorageCheckMs <= 1500);
check("Electron 3 跑 window P50 ≤1.5s", metrics.electron.windowShownMs.p50 <= 1500);
check("Electron 3 跑 ui P50 ≤1.5s", metrics.electron.uiReadyMs.p50 <= 1500);
check("备份预算 ≤600s", metrics.backupRestore.backup_ms <= 600000);
check("恢复 staging ≤900s", metrics.backupRestore.stage_restore_ms <= 900000);
check("备份 manifest 10 文件", metrics.backupRestore.manifest_files === 10);

const storeWrite = read("crates/aether-store/src/write_queue.rs");
const storeOpen = read("crates/aether-store/src/store.rs");
const layout = read("crates/aether-store/src/layout.rs");
const health = read("crates/aether-app/src/core_health.rs");
const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
check("Store::open_deferred_check", storeOpen.includes("open_deferred_check"));
check("open_generation_deferred", storeWrite.includes("open_generation_deferred"));
check("WriteQueue pause/resume", storeWrite.includes("pause_writes") && storeWrite.includes("resume_writes"));
check("严格 sidecar", layout.includes("clean_shutdown_is_strict"));
check("并行 quick_check", health.includes("quick_check_path") && health.includes("STARTUP_CHECK_CONCURRENCY"));
check("动态 storage_check", health.includes("StorageCheckCell") && health.includes("with_storage_check_cell"));
check("动态 maintenance", health.includes("with_maintenance_cell") && metrics.core.finalStorageCheck.write_gate === "open");
check("诊断物理 generation", diagnostics.includes("copy_generation_files") && diagnostics.includes("generation/aether.db"));
check(
  "壳层 uiReady 兜底",
  read("apps/desktop/electron/app.ts").includes('source: "shell"'),
);

const run = (args) => {
  const result = spawnSync(process.env.CARGO ?? "cargo", args, {
    cwd: root,
    stdio: "inherit",
    env: { ...process.env, CARGO_BUILD_JOBS: "1", CARGO_INCREMENTAL: "0" },
  });
  check(`cargo ${args.join(" ")}`, result.status === 0);
};
run(["test", "-p", "aether-store", "--test", "m10_09_sharding", "-j", "1", "--", "--test-threads=1"]);
run(["test", "-p", "aether-store", "--test", "m10_10_restore_kill", "-j", "1", "--", "--test-threads=1"]);
run(["test", "-p", "aether-app", "--test", "m5_03_encrypted_backup", "-j", "1", "--", "--test-threads=1"]);
const dataDir = process.env.AETHER_M10_10_DATA_DIR;
if (dataDir && existsSync(dataDir)) {
  run(["test", "-p", "aether-app", "--test", "m10_10_startup", "-j", "1", "--", "--ignored", "--nocapture"]);
} else {
  console.log("NOTE  未设置 AETHER_M10_10_DATA_DIR，跳过 5.22GiB core 启动复测");
}

if (failures > 0) {
  console.error(`[m10-10] FAIL（${failures} 项）`);
  process.exit(1);
}
console.log("AETHER_M10_10 PASS");
