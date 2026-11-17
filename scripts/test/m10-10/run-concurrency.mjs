#!/usr/bin/env node
/**
 * M10-10 启动检查并发矩阵复跑入口（ADR-039 D-039-3）。
 *
 * 用法：
 *   node scripts/test/m10-10/run-concurrency.mjs [--dataset DIR] [--out FILE] [--levels 2,4,8]
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let dataset = process.env.AETHER_M10_10_DATA_DIR ?? path.join(os.tmpdir(), "opencode", "m10-09", "generation");
let out = path.join(root, "dist-out", "m10-10", "concurrency-matrix.json");
let levels = process.env.AETHER_M10_10_CONCURRENCY ?? "2,4,8";
const argv = process.argv.slice(2);
for (let index = 0; index < argv.length; index += 1) {
  const next = () => {
    index += 1;
    if (index >= argv.length) throw new Error(`${argv[index - 1]} 缺少参数`);
    return argv[index];
  };
  switch (argv[index]) {
    case "--dataset":
      dataset = path.resolve(root, next());
      break;
    case "--out":
      out = path.resolve(root, next());
      break;
    case "--levels":
      levels = next();
      break;
    default:
      break;
  }
}
if (!existsSync(dataset)) {
  console.error(`[m10-10-concurrency] 数据集不存在：${dataset}`);
  process.exit(1);
}
const result = spawnSync(
  process.env.CARGO ?? "cargo",
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
    cwd: root,
    stdio: "inherit",
    env: {
      ...process.env,
      AETHER_M10_10_DATA_DIR: dataset,
      AETHER_M10_10_CONCURRENCY: levels,
      AETHER_M10_10_CONCURRENCY_OUT: out,
      CARGO_BUILD_JOBS: "1",
      CARGO_INCREMENTAL: "0",
    },
  },
);
if (result.status !== 0) {
  console.error("[m10-10-concurrency] FAIL");
  process.exit(result.status ?? 1);
}
console.log(`[m10-10-concurrency] PASS：${out}`);
