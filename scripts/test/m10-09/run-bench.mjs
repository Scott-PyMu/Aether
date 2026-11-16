#!/usr/bin/env node
/**
 * `pnpm m10-09:bench` —— M10-09 标准性能复测（10 并发、默认 600s）。
 *
 * 前置：
 *   node scripts/test/m10-08/gen-events-dataset.mjs --out dist-out/m10-09/dataset --force
 *   node scripts/test/m10-09/build-generation.mjs --out dist-out/m10-09/generation --force
 *
 * 可选环境变量：
 *   AETHER_M10_09_SECONDS=600
 *   AETHER_M10_09_DATA_DIR=dist-out/m10-09/generation
 *   AETHER_M10_09_OUT=dist-out/m10-09/bench-600s.json
 */
import { execFileSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const env = {
  ...process.env,
  AETHER_M10_09_SECONDS: process.env.AETHER_M10_09_SECONDS ?? "600",
  AETHER_M10_09_DATA_DIR:
    process.env.AETHER_M10_09_DATA_DIR ?? path.join(root, "dist-out", "m10-09", "generation"),
  AETHER_M10_09_OUT:
    process.env.AETHER_M10_09_OUT ?? path.join(root, "dist-out", "m10-09", "bench-600s.json"),
};
const cargo = process.env.CARGO ?? "cargo";
console.log(
  `[m10-09-bench] seconds=${env.AETHER_M10_09_SECONDS} data=${env.AETHER_M10_09_DATA_DIR}`,
);
execFileSync(
  cargo,
  [
    "test",
    "-p",
    "aether-store",
    "--test",
    "m10_09_bench",
    "-j",
    "1",
    "--",
    "--ignored",
    "--nocapture",
  ],
  { cwd: root, stdio: "inherit", env },
);
