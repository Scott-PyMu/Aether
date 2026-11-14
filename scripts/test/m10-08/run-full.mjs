#!/usr/bin/env node
/**
 * M10-08 全量复跑编排（ADR-038 D-038-1/D-038-3）。
 *
 * 顺序执行：
 *   1. 数据集构造（~5GiB / 20M+ rows，固定 seed）；
 *   2. 3 次单库压测（10 并发写入 ≥10min + 补读 + quick_check + 备份/恢复 + 保留模拟）；
 *   3. 分片数据集构造（N=8，同数据集/机器/并发）；
 *   4. 3 次分片对照压测；
 *   5. 真实 Electron + daemon 5GB 冷启动 3 次；
 *   6. 汇总 `docs/M10-08-metrics.json`。
 *
 * 预计耗时：单库 3×~12min + 分片 3×~12min + 数据集/分片构造 ~15min。
 * 日志写入 `dist-out/m10-08/logs/`；中断后重跑会复用已完成的数据集/分片，
 * 但压测结果文件会覆盖同 run-index。
 *
 * 用法：
 *   node scripts/test/m10-08/run-full.mjs
 *   node scripts/test/m10-08/run-full.mjs --runs 1 --write-seconds 30 --read-seconds 5 \
 *     --target-gib 0.125 --min-rows 500000 --sessions 10 --shards 2   # 冒烟
 */
import { spawn } from "node:child_process";
import { createWriteStream, existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { ensureDir, repoRootFrom } from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
let logsDir = path.join(repoRoot, "dist-out", "m10-08", "logs");

function parseArgs(argv) {
  const options = {
    workDir: path.join(repoRoot, "dist-out", "m10-08"),
    runs: 3,
    writeSeconds: 600,
    readSeconds: 60,
    shards: 8,
    targetGib: 5,
    minRows: 20_000_000,
    sessions: 100,
    skipGenerate: false,
    skipSingle: false,
    skipSharded: false,
    skipStartup: false,
    forceGenerate: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--work-dir":
        options.workDir = path.resolve(repoRoot, next());
        break;
      case "--runs":
        options.runs = Number(next());
        break;
      case "--write-seconds":
        options.writeSeconds = Number(next());
        break;
      case "--read-seconds":
        options.readSeconds = Number(next());
        break;
      case "--shards":
        options.shards = Number(next());
        break;
      case "--target-gib":
        options.targetGib = Number(next());
        break;
      case "--min-rows":
        options.minRows = Number(next());
        break;
      case "--sessions":
        options.sessions = Number(next());
        break;
      case "--skip-generate":
        options.skipGenerate = true;
        break;
      case "--force-generate":
        options.forceGenerate = true;
        break;
      case "--skip-single":
        options.skipSingle = true;
        break;
      case "--skip-sharded":
        options.skipSharded = true;
        break;
      case "--skip-startup":
        options.skipStartup = true;
        break;
      case "--help":
      case "-h":
        console.log(
          "用法：node scripts/test/m10-08/run-full.mjs [--runs N] [--write-seconds N] [--read-seconds N] " +
            "[--shards N] [--skip-generate] [--skip-single] [--skip-sharded] [--skip-startup]",
        );
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  return options;
}

function timestamp() {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

function run(label, args) {
  return new Promise((resolve, reject) => {
    const logPath = path.join(logsDir, `${label}.log`);
    const stream = createWriteStream(logPath, { flags: "a" });
    const child = spawn(process.execPath, args, {
      cwd: repoRoot,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const write = (chunk) => {
      stream.write(chunk);
      process.stdout.write(chunk);
    };
    child.stdout.on("data", write);
    child.stderr.on("data", write);
    child.on("error", (error) => {
      stream.end();
      reject(error);
    });
    child.on("close", (code) => {
      stream.end();
      if (code === 0) {
        console.log(`\n[run-full] ${label} 完成（日志：${path.relative(repoRoot, logPath)}）`);
        resolve();
      } else {
        reject(new Error(`${label} 退出码 ${code}（日志：${path.relative(repoRoot, logPath)}）`));
      }
    });
  });
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (!Number.isInteger(options.runs) || options.runs < 1) throw new Error("--runs 必须 ≥1");
  const resultsDir = path.join(options.workDir, "results");
  logsDir = path.join(options.workDir, "logs");
  ensureDir(logsDir);
  ensureDir(resultsDir);
  const datasetDir = path.join(options.workDir, "dataset");
  const shardsDir = path.join(options.workDir, "shards");
  const benchmark = path.join(here, "bench-events.mjs");

  if (!options.skipGenerate) {
    const args = [
      path.join(here, "gen-events-dataset.mjs"),
      "--out",
      datasetDir,
      "--target-gib",
      String(options.targetGib),
      "--min-rows",
      String(options.minRows),
      "--sessions",
      String(options.sessions),
    ];
    if (options.forceGenerate) args.push("--force");
    await run("01-generate", args);
  }

  if (!options.skipSingle) {
    for (let index = 1; index <= options.runs; index += 1) {
      const args = [
        benchmark,
        "--dataset",
        datasetDir,
        "--out",
        resultsDir,
        "--run-index",
        String(index),
        "--label",
        "single",
        "--write-seconds",
        String(options.writeSeconds),
        "--read-seconds",
        String(options.readSeconds),
        "--quick-check-runs",
        "1",
      ];
      // 保留策略清理模拟只跑第 1 次（结果可复现；后续 run 复用同一清理结论）。
      if (index > 1) args.push("--skip-retention");
      await run(`02-single-run${index}`, args);
    }
  }

  if (!options.skipSharded) {
    if (!existsSync(path.join(shardsDir, "shards.json"))) {
      await run("03-build-shards", [
        path.join(here, "build-shards.mjs"),
        "--dataset",
        datasetDir,
        "--out",
        shardsDir,
        "--shards",
        String(options.shards),
      ]);
    }
    for (let index = 1; index <= options.runs; index += 1) {
      await run(`04-sharded-run${index}`, [
        benchmark,
        "--dataset",
        datasetDir,
        "--out",
        resultsDir,
        "--shards-dir",
        shardsDir,
        "--run-index",
        String(index),
        "--label",
        "sharded",
        "--shards",
        String(options.shards),
        "--write-seconds",
        String(options.writeSeconds),
        "--read-seconds",
        String(options.readSeconds),
        "--quick-check-runs",
        "1",
      ]);
    }
  }

  if (!options.skipStartup) {
    await run("05-startup-e2e", [
      path.join(here, "startup-e2e.mjs"),
      "--dataset",
      datasetDir,
      "--out",
      resultsDir,
      "--runs",
      String(options.runs),
    ]);
  }

  await run("06-aggregate", [path.join(here, "aggregate-m10-08.mjs"), "--results", resultsDir]);
  console.log(`\n[run-full] 完成：${timestamp()}`);
}

main().catch((error) => {
  console.error(`[m10-08-run-full] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
