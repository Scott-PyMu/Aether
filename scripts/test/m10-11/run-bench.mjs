#!/usr/bin/env node
/**
 * `pnpm m10-11:bench` —— 删除事务档位矩阵（500/1000/2000/5000）与保留路径基准。
 *
 * 环境变量（可选）：
 * - `AETHER_M10_11_GENERATION`：5.22GiB generation 副本（`CURRENT` 所在目录）；
 *   未设置时使用合成数据集（每档 5 万行）。
 * - `AETHER_M10_11_BENCH_ROOT`：临时数据目录根（HDD/SMR 复测时指向目标盘）。
 * - `AETHER_M10_11_EVIDENCE_DIR`：矩阵 JSON 输出目录（默认 dist-out/m10-11）。
 * - `AETHER_M10_11_TIERS`：逗号分隔的档位（默认 500,1000,2000,5000）。
 *
 * 说明：release 构建；结果仅作矩阵/选档证据，不改写 `docs/M10-11-metrics.json`。
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const evidenceDir =
  process.env.AETHER_M10_11_EVIDENCE_DIR ?? path.join(root, "dist-out", "m10-11");
mkdirSync(evidenceDir, { recursive: true });

const env = {
  ...process.env,
  AETHER_M10_11_EVIDENCE_DIR: evidenceDir,
};

console.log(`[m10-11] evidence dir: ${evidenceDir}`);
if (env.AETHER_M10_11_GENERATION) {
  console.log(`[m10-11] generation: ${env.AETHER_M10_11_GENERATION}`);
}
if (env.AETHER_M10_11_BENCH_ROOT) {
  console.log(`[m10-11] bench root: ${env.AETHER_M10_11_BENCH_ROOT}`);
}

try {
  execFileSync(
    "cargo",
    [
      "test",
      "--release",
      "-p",
      "aether-store",
      "--test",
      "m10_11_bench",
      "-j",
      "1",
      "--",
      "--test-threads=1",
      "--nocapture",
    ],
    { cwd: root, stdio: "inherit", env },
  );
} catch (error) {
  console.error(`[m10-11] bench 失败：退出码 ${error.status ?? error.message}`);
  process.exit(1);
}

const matrixPath = path.join(evidenceDir, "delete_batch_matrix.json");
if (!existsSync(matrixPath)) {
  console.error(`[m10-11] 缺少矩阵产物：${matrixPath}`);
  process.exit(1);
}
const matrix = JSON.parse(readFileSync(matrixPath, "utf8"));
console.log("\n===== m10-11 delete batch matrix =====");
for (const entry of matrix.tiers ?? []) {
  console.log(
    `tier=${entry.tier} delete_p50=${entry.delete.p50_ms}ms p95=${entry.delete.p95_ms}ms ` +
      `max=${entry.delete.max_ms}ms compliant=${entry.compliant}`,
  );
}
console.log(`chosen_tier=${matrix.chosen_tier}`);
