#!/usr/bin/env node
/**
 * M10-08 分片并行 `quick_check` 对照（ADR-039 草案 D-039-2 第 3 项）。
 *
 * 只读分片数据集，**不改变产品 schema/命令**；用于回答：
 * 「8 个分片顺序检查 vs 并行检查」的墙钟时间。
 *
 * 用法：
 *   node scripts/test/m10-08/parallel-quick-check.mjs --shards-dir dist-out/m10-08/shards --shards 8
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { readJson, repoRootFrom, summarize, writeJson } from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const worker = path.join(here, "bench-worker.mjs");

function parseArgs(argv) {
  const options = {
    shardsDir: path.join(repoRoot, "dist-out", "m10-08", "shards"),
    shards: 8,
    repeat: 1,
    out: null,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--shards-dir":
        options.shardsDir = path.resolve(repoRoot, next());
        break;
      case "--shards":
        options.shards = Number(next());
        break;
      case "--repeat":
        options.repeat = Number(next());
        break;
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--help":
      case "-h":
        console.log(
          "用法：node scripts/test/m10-08/parallel-quick-check.mjs [--shards-dir DIR] [--shards N] [--repeat N]",
        );
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  return options;
}

function runOne(dbPath) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [worker, "--mode", "quick-check", "--db", dbPath], {
      cwd: repoRoot,
      stdio: ["ignore", "pipe", "inherit"],
    });
    let stdout = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk.toString();
    });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) {
        reject(new Error(`quick-check 退出码 ${code}：${dbPath}`));
        return;
      }
      try {
        resolve(JSON.parse(stdout.trim().split(/\r?\n/).filter(Boolean).at(-1)));
      } catch (error) {
        reject(new Error(`quick-check 输出不是 JSON：${stdout.slice(0, 500)}（${error.message}）`));
      }
    });
  });
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const manifestPath = path.join(options.shardsDir, "shards.json");
  if (!existsSync(manifestPath)) throw new Error(`分片 manifest 不存在：${options.shardsDir}`);
  const manifest = readJson(manifestPath);
  if (manifest.shards !== options.shards) {
    throw new Error(`分片数不一致：manifest=${manifest.shards} / --shards=${options.shards}`);
  }
  const runs = [];
  for (let iteration = 1; iteration <= options.repeat; iteration += 1) {
    const started = performance.now();
    const results = await Promise.all(
      manifest.files.map((entry) => runOne(path.join(options.shardsDir, entry.db))),
    );
    const wallMs = performance.now() - started;
    runs.push({
      iteration,
      wallMs: Number(wallMs.toFixed(3)),
      sumSequentialMs: Number(results.reduce((sum, entry) => sum + entry.totalMs, 0).toFixed(3)),
      maxShardMs: Number(Math.max(...results.map((entry) => entry.totalMs)).toFixed(3)),
      perShard: results.map((entry, index) => ({
        shard: index,
        openMs: entry.openMs,
        quickCheckMs: entry.quickCheckMs,
        totalMs: entry.totalMs,
        ok: entry.ok,
      })),
    });
    console.log(
      `[m10-08-parallel-qc] ${iteration}/${options.repeat}: wall=${runs.at(-1).wallMs}ms / ` +
        `sum=${runs.at(-1).sumSequentialMs}ms / maxShard=${runs.at(-1).maxShardMs}ms`,
    );
  }
  const wall = summarize(runs.map((entry) => entry.wallMs));
  const sum = summarize(runs.map((entry) => entry.sumSequentialMs));
  const result = {
    task: "M10-08",
    adr: "ADR-039（草案）",
    shards: options.shards,
    runs,
    wallMs: wall,
    sumSequentialMs: sum,
    speedup: sum.p50 > 0 ? Number((sum.p50 / wall.p50).toFixed(2)) : null,
    ok: runs.every((entry) => entry.perShard.every((shard) => shard.ok)),
  };
  if (options.out) writeJson(options.out, result);
  console.log(JSON.stringify(result, null, 2));
}

main().catch((error) => {
  console.error(`[m10-08-parallel-qc] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
