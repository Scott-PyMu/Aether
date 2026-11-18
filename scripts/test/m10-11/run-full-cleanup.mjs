#!/usr/bin/env node
/**
 * `pnpm m10-11:full` —— 5.22GiB 标准 generation 的全量历史清理/维护态 VACUUM
 * 或 1M 行日常增量预算复测（release；ignored 测试）。
 *
 * 用法：
 *   pnpm m10-11:full -- --scenario=full
 *   pnpm m10-11:full -- --scenario=vacuum
 *   pnpm m10-11:full -- --scenario=daily
 *
 * 环境变量：
 * - `AETHER_M10_11_GENERATION`：generation 副本（`CURRENT` 所在目录）；full/vacuum 必需。
 * - `AETHER_M10_11_GENERATION_IS_COPY=1`：直接使用该副本（不再复制）。
 * - `AETHER_M10_11_BENCH_ROOT`：复制/归档临时目录根。
 * - `AETHER_M10_11_EVIDENCE_DIR`：证据 JSON 输出目录（默认 dist-out/m10-11）。
 */
import { execFileSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { mkdirSync } from "node:fs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const scenarioArg = process.argv.find((value) => value.startsWith("--scenario="));
const scenario = scenarioArg?.slice("--scenario=".length) ?? "full";
const testName = {
  full: "full_history_cleanup_on_standard_generation",
  vacuum: "maintenance_vacuum_on_standard_generation",
  daily: "daily_incremental_cleanup_within_five_minutes",
}[scenario];
if (!testName) {
  console.error(`未知 scenario=${scenario}（full|vacuum|daily）`);
  process.exit(2);
}
if ((scenario === "full" || scenario === "vacuum") && !process.env.AETHER_M10_11_GENERATION) {
  console.error(`scenario=${scenario} 需要 AETHER_M10_11_GENERATION`);
  process.exit(2);
}
const evidenceDir =
  process.env.AETHER_M10_11_EVIDENCE_DIR ?? path.join(root, "dist-out", "m10-11");
mkdirSync(evidenceDir, { recursive: true });
const env = {
  ...process.env,
  AETHER_M10_11_EVIDENCE_DIR: evidenceDir,
};

console.log(`[m10-11] scenario=${scenario} test=${testName}`);
console.log(`[m10-11] evidence dir=${evidenceDir}`);

try {
  execFileSync(
    "cargo",
    [
      "test",
      "--release",
      "-p",
      "aether-app",
      "--test",
      "m10_11_retention",
      testName,
      "--",
      "--ignored",
      "--test-threads=1",
      "--nocapture",
    ],
    { cwd: root, stdio: "inherit", env },
  );
} catch (error) {
  console.error(`[m10-11] ${scenario} 失败：退出码 ${error.status ?? error.message}`);
  process.exit(1);
}
console.log(`[m10-11] ${scenario} 完成；证据见 ${evidenceDir}`);
