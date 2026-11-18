#!/usr/bin/env node
/**
 * M10-14 / ADR-040 §4.3：dirty 1/2/4/8 × 冷/热 × 5.22 GiB × release 启动矩阵。
 *
 * 每格至少 3 次：冷 = generation 复制后首次启动；热 = 紧接第二次启动。
 * 结果写 `dist-out/m10-14/perf-matrix.json`（含 P50/P95/max/失败数）。
 *
 * 用法：
 *   node scripts/test/m10-14/perf-matrix.mjs --dataset <generation> --runs 3
 *   node scripts/test/m10-14/perf-matrix.mjs --dataset <generation> --dirty 1,2 --runs 1
 */
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const defaultDataset =
  process.env.AETHER_M10_14_DATA_DIR ??
  path.join(os.tmpdir(), "opencode", "m10-09", "generation");

const options = {
  dataset: defaultDataset,
  work: path.join(root, "dist-out", "m10-14", "work"),
  out: path.join(root, "dist-out", "m10-14", "perf-matrix.json"),
  dirty: [1, 2, 4, 8],
  runs: 3,
  keep: false,
};
for (let index = 2; index < process.argv.length; index += 1) {
  const arg = process.argv[index];
  const next = () => {
    index += 1;
    if (index >= process.argv.length) throw new Error(`${arg} 缺少参数`);
    return process.argv[index];
  };
  switch (arg) {
    case "--dataset":
      options.dataset = path.resolve(root, next());
      break;
    case "--work":
      options.work = path.resolve(root, next());
      break;
    case "--out":
      options.out = path.resolve(root, next());
      break;
    case "--dirty":
      options.dirty = next()
        .split(",")
        .map((value) => Number(value.trim()))
        .filter((value) => Number.isInteger(value) && value >= 0 && value <= 8);
      break;
    case "--runs":
      options.runs = Number(next());
      break;
    case "--keep":
      options.keep = true;
      break;
    default:
      throw new Error(`未知参数：${arg}`);
  }
}
if (!existsSync(path.join(options.dataset, "CURRENT"))) {
  throw new Error(`dataset 不是 generation（缺 CURRENT）：${options.dataset}`);
}
if (!Number.isInteger(options.runs) || options.runs < 1) throw new Error("--runs 必须 ≥1");

const cargo = process.env.CARGO ?? "cargo";
const baseDir = path.join(options.work, "base");
const env = {
  ...process.env,
  CARGO_BUILD_JOBS: "1",
  CARGO_INCREMENTAL: "0",
};

function resolveTestBinary() {
  console.log("[perf] 构建 release 测试二进制（仅一次）");
  const output = execFileSync(
    cargo,
    [
      "test",
      "--release",
      "-p",
      "aether-app",
      "--test",
      "m10_14_perf",
      "-j",
      "1",
      "--no-run",
      "--message-format=json",
    ],
    {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
      env,
      maxBuffer: 64 * 1024 * 1024,
      timeout: 0,
    },
  );
  const executables = output
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter((entry) => entry?.executable && entry?.target?.name === "m10_14_perf")
    .map((entry) => entry.executable);
  if (executables.length === 0) throw new Error("未找到 m10_14_perf 测试二进制");
  return executables[executables.length - 1];
}

const testBinary = resolveTestBinary();

function runBinary(testName, extraEnv, label) {
  console.log(`[perf] ${label}`);
  execFileSync(testBinary, [testName, "--ignored", "--nocapture", "--test-threads=1"], {
    cwd: root,
    stdio: "inherit",
    env: { ...env, ...extraEnv },
    timeout: 0,
  });
}

function runCell(dir, dirtyCount, cold, run) {
  const dirtyIds = Array.from({ length: dirtyCount }, (_, index) => index);
  const out = path.join(options.work, `cell-${dirtyCount}-${cold ? "cold" : "hot"}-${run}.json`);
  rmSync(out, { force: true });
  runBinary(
    "selective_startup_perf_cell",
    {
      AETHER_M10_14_DATA_DIR: dir,
      AETHER_M10_14_DIRTY: dirtyIds.join(","),
      AETHER_M10_14_COLD: cold ? "1" : "0",
      AETHER_M10_14_RUN: String(run),
      AETHER_M10_14_OUT: out,
    },
    `dirty=${dirtyCount} (${dirtyIds.join(",")}) cold=${cold} run=${run}`,
  );
  return JSON.parse(readFileSync(out, "utf8")).cell;
}

function summarize(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const at = (percentile) => {
    if (sorted.length === 0) return null;
    const index = Math.min(
      sorted.length - 1,
      Math.max(0, Math.ceil((percentile / 100) * sorted.length) - 1),
    );
    return sorted[index];
  };
  return {
    count: sorted.length,
    min: sorted[0] ?? null,
    p50: at(50),
    p95: at(95),
    max: sorted[sorted.length - 1] ?? null,
    mean: sorted.length === 0 ? null : sorted.reduce((sum, value) => sum + value, 0) / sorted.length,
  };
}

mkdirSync(options.work, { recursive: true });
if (!existsSync(baseDir)) {
  console.log(`[perf] 复制基准 generation → ${baseDir}`);
  cpSync(options.dataset, baseDir, { recursive: true });
}
// 准备干净 sidecar（只做一次；后续每格复制该基准）。
runBinary(
  "prepare_clean_sidecar",
  { AETHER_M10_14_DATA_DIR: baseDir },
  "prepare clean sidecar",
);

const results = [];
for (const dirty of options.dirty) {
  for (let run = 1; run <= options.runs; run += 1) {
    const coldDir = path.join(options.work, `d${dirty}-r${run}-cold`);
    rmSync(coldDir, { recursive: true, force: true });
    console.log(`[perf] 复制冷数据集 → ${coldDir}`);
    cpSync(baseDir, coldDir, { recursive: true });
    try {
      results.push(runCell(coldDir, dirty, true, run));
      results.push(runCell(coldDir, dirty, false, run));
    } finally {
      if (!options.keep) rmSync(coldDir, { recursive: true, force: true });
    }
  }
}

const matrix = {};
for (const dirty of options.dirty) {
  const cells = results.filter((cell) => cell.dirty.length === dirty);
  matrix[dirty] = {
    dirty,
    cold: summarize(cells.filter((cell) => cell.cold).map((cell) => cell.startupCheckCompletedAtMs)),
    hot: summarize(cells.filter((cell) => !cell.cold).map((cell) => cell.startupCheckCompletedAtMs)),
    failures: cells.filter((cell) => cell.state !== "ok" || cell.failureCount > 0).length,
    samples: cells,
  };
}

const fullBaseline = matrix[8]?.hot?.p50 ?? null;
const report = {
  task: "M10-14",
  adr: "ADR-040 v1.0 §4.3",
  recorded_at: new Date().toISOString(),
  dataset: options.dataset,
  runs_per_cell: options.runs,
  matrix,
  targets: {
    fullBaselineHotP50Ms: fullBaseline,
    gain: Object.fromEntries(
      options.dirty.map((dirty) => {
        const median = matrix[dirty]?.hot?.p50;
        return [
          dirty,
          fullBaseline && median ? Number((1 - median / fullBaseline).toFixed(4)) : null,
        ];
      }),
    ),
    p95TargetMs: 25000,
    p95HardMs: 30000,
  },
};
mkdirSync(path.dirname(options.out), { recursive: true });
writeFileSync(options.out, `${JSON.stringify(report, null, 2)}\n`, "utf8");
console.log(`[perf] 结果写入 ${options.out}`);
