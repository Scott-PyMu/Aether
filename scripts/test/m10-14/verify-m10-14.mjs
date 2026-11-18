#!/usr/bin/env node
/**
 * M10-14 验证入口（ADR-040 v1.0）。
 *
 * 模式：
 *   node scripts/test/m10-14/verify-m10-14.mjs                  # 当前实现（默认）
 *   node scripts/test/m10-14/verify-m10-14.mjs --rollback=level1 # 选择性关闭（编译期 feature）
 *   node scripts/test/m10-14/verify-m10-14.mjs --rollback=level2 # 固定 ADR-039 fixture 行为集
 *
 * 边界：`verify:m10-10` 是 ADR-039 基线模式（只校验基线哈希/来源/断言）；
 * 当前实现（选择性/health v2/运行期调度）由本脚本验证。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const mode = process.argv.find((arg) => arg.startsWith("--rollback="))?.split("=")[1] ?? "current";
if (!["current", "level1", "level2"].includes(mode)) {
  console.error(`未知模式：${mode}（仅 current/level1/level2）`);
  process.exit(2);
}
let failures = 0;
const check = (label, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? `：${detail}` : ""}`);
  if (!ok) failures += 1;
};
const read = (relative) => readFileSync(path.join(root, relative), "utf8");
const json = (relative) => JSON.parse(read(relative));
const exists = (relative) => existsSync(path.join(root, relative));
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const cargo = process.env.CARGO ?? "cargo";

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

const runCargoTest = (label, crate, target, extraArgs = [], extraEnv = {}) =>
  run(label, cargo, [
    "test",
    "-p",
    crate,
    "--test",
    target,
    "-j",
    "1",
    ...extraArgs,
    "--",
    "--test-threads=1",
  ], extraEnv);

const runBaseline = () =>
  run("verify:m10-10（ADR-039 基线模式）", pnpm, ["verify:m10-10"]);

if (mode === "level1" || mode === "level2") {
  console.log(`M10-14 回退演练模式：${mode}`);
  run(
    "cargo m10_14_level1（选择性关闭）",
    cargo,
    [
      "test",
      "-p",
      "aether-app",
      "--features",
      "startup-check-level1",
      "--test",
      "m10_14_level1",
      "-j",
      "1",
      "--",
      "--test-threads=1",
    ],
  );
  if (mode === "level2") {
    run("Level 2 固定 fixture 行为测试", process.execPath, [
      "scripts/test/m10-14/level2-fixture.mjs",
    ]);
  }
  runBaseline();
  if (failures > 0) {
    console.error(`[m10-14:${mode}] FAIL（${failures} 项）`);
    process.exit(1);
  }
  console.log(`AETHER_M10_14_${mode.toUpperCase()} PASS`);
  process.exit(0);
}

// ── 当前实现静态断言 ────────────────────────────────────────────────────────
const layout = read("crates/aether-store/src/layout.rs");
const writeQueue = read("crates/aether-store/src/write_queue.rs");
const coreHealth = read("crates/aether-app/src/core_health.rs");
const storageCheck = read("crates/aether-app/src/storage_check.rs");
const importControl = read("crates/aether-app/src/import_control.rs");
const shutdown = read("crates/aether-app/src/shutdown.rs");
const healthTs = read("apps/desktop/src/health.ts");
const healthMonitor = read("apps/desktop/src/HealthMonitor.tsx");
const appTs = read("apps/desktop/electron/app.ts");
const zh = json("apps/desktop/src/i18n/locales/zh-CN.json");
const en = json("apps/desktop/src/i18n/locales/en-US.json");
const pkg = json("package.json");

check("sidecar format_version 常量", layout.includes("SIDECAR_FORMAT_VERSION"));
check("选择性编译期开关", layout.includes("STARTUP_CHECK_SELECTIVE_ENABLED"));
check("主库单独变脏", layout.includes("main_fingerprint_mismatch"));
check("仅变化分片计划", layout.includes("changed.len() == shard_count"));
check("严格匹配 background_check_completed", layout.includes("background_check_incomplete"));
check("严格匹配 WAL", layout.includes("wal_not_checkpointed"));
check("严格匹配 schema", layout.includes("schema_version_mismatch"));
check("分片集合校验", layout.includes("shard_set_mismatch"));
check("full_check_due None 基线", writeQueue.includes("None => true"));
check(
  "health v2 字段",
  ["contract_version", "planned_shards", "skipped_shards", "full_coverage"].every((field) =>
    coreHealth.includes(field),
  ),
);
check("运行期调度器", coreHealth.includes("spawn_runtime_scheduler") && coreHealth.includes("scheduler_loop"));
check("调度器监督失败降级", coreHealth.includes("storage_check_scheduler_supervision_failed"));
check("检查超时", coreHealth.includes("storage_check_timeout"));
check("检查 epoch/迟到结果", coreHealth.includes("invalidate_epoch") && coreHealth.includes("is_current"));
check("shutdown grace", coreHealth.includes("shutdown_cancel") && storageCheck.includes("SHUTDOWN_CHECK_GRACE"));
check(
  "导入窗口",
  storageCheck.includes("ImportActivity") &&
    importControl.includes("with_import_activity") &&
    importControl.includes("self.activity.set(true)"),
);
check(
  "退出编排取消检查",
  shutdown.includes("install_storage_check") && shutdown.includes("disable_clean_sidecar"),
);
check(
  "UI v2 文案",
  healthMonitor.includes("storage_check_progress_v2") &&
    healthMonitor.includes("skipped_shards") &&
    healthMonitor.includes("data-contract-version"),
);
check("UI 旧 daemon 兼容", healthMonitor.includes("contract_version === 2"));
check("无新增 testid", !healthMonitor.includes("storage-check-skip"));
check("health.ts 新字段", [
  "contract_version",
  "planned_shards",
  "skipped_shards",
  "full_coverage",
].every((field) => healthTs.includes(field)));
check(
  "app.ts 指标拆分",
  ["startupCheckCompletedAt", "fullCoverageCheckCompletedAt", "backgroundFullCheckDurationMs", "periodicFullCheckDurationMs"].every(
    (field) => appTs.includes(field),
  ) && appTs.includes("storageCheckCompletedAt"),
);
check(
  "i18n 键集合一致（新增 v2 键）",
  Object.keys(zh).sort().join("\n") === Object.keys(en).sort().join("\n") &&
    zh["health.storage_check_progress_v2"] !== undefined &&
    en["health.storage_check_progress_v2"] !== undefined,
);
check(
  "package.json 注册三模式",
  pkg.scripts?.["verify:m10-14"] !== undefined &&
    pkg.scripts?.["verify:m10-14:level1"] !== undefined &&
    pkg.scripts?.["verify:m10-14:level2"] !== undefined,
);
check("补充证据存在", exists("docs/M10-10-补充证据.md"));
check("补充 metrics 存在", exists("docs/M10-10-补充-metrics.json"));
check("ADR-039 基线存在", exists("scripts/test/m10-10/adr039-baseline.json"));

// ── 契约计数 ────────────────────────────────────────────────────────────────
const manifest = json("packages/protocol/src/command-manifest.json");
const commands = Array.isArray(manifest) ? manifest : manifest.commands;
check("命令面 87", Array.isArray(commands) && commands.length === 87, `${commands?.length}`);
const errorCodes = json("packages/protocol/src/error_codes.json");
check("IpcErrorCode 32", (errorCodes.ipc ?? []).length === 32, `${errorCodes.ipc?.length}`);
check("业务码 35", (errorCodes.business ?? []).length === 35, `${errorCodes.business?.length}`);
check("迁移 0014 存在", exists("migrations/0014_session_shard.sql"));
check("迁移 0015 不存在", !exists("migrations/0015_session_shard.sql"));

// ── 行为测试 ────────────────────────────────────────────────────────────────
runCargoTest("cargo m10_14_plan", "aether-app", "m10_14_plan");
runCargoTest("cargo m10_14_startup", "aether-app", "m10_14_startup");
runCargoTest("cargo m10_14_periodic", "aether-app", "m10_14_periodic");
runCargoTest("cargo m10_14_lifecycle", "aether-app", "m10_14_lifecycle");
runCargoTest("cargo m10_14_flags", "aether-app", "m10_14_flags");
runCargoTest("cargo m10_14_fault", "aether-app", "m10_14_fault");
runCargoTest("cargo m10_09_sharding", "aether-store", "m10_09_sharding");
run(
  "desktop storage_check UI tests",
  pnpm,
  ["--filter", "@aether/desktop", "test", "m10_10_storage_check.test.tsx"],
);

// ── 验证资产 ────────────────────────────────────────────────────────────────
runBaseline();
run("protocol check", pnpm, ["--filter", "@aether/protocol", "check"]);
run("verify:error-codes", pnpm, ["verify:error-codes"]);
run("verify:m1-03", pnpm, ["verify:m1-03"]);
run("verify:m9-01-baseline", pnpm, ["verify:m9-01-baseline"]);

if (failures > 0) {
  console.error(`[m10-14] FAIL（${failures} 项）`);
  process.exit(1);
}
console.log("AETHER_M10_14 PASS");
