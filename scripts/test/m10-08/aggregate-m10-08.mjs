#!/usr/bin/env node
/**
 * M10-08 指标汇总与二选一判定（ADR-038 D-038-2/D-038-3）。
 *
 * 读取 3 次单库 metrics + 3 次分片 metrics + startup-e2e，输出
 * `docs/M10-08-metrics.json`（小体积、可入库）与 Markdown 判定表。
 *
 * 用法：
 *   node scripts/test/m10-08/aggregate-m10-08.mjs
 *   node scripts/test/m10-08/aggregate-m10-08.mjs --results dist-out/m10-08/results
 */
import { existsSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import {
  GIB,
  THRESHOLDS,
  ensureDir,
  formatBytes,
  readJson,
  repoRootFrom,
  summarize,
  writeJson,
} from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);

function parseArgs(argv) {
  const options = {
    results: path.join(repoRoot, "dist-out", "m10-08", "results"),
    out: path.join(repoRoot, "docs", "M10-08-metrics.json"),
    evidence: path.join(repoRoot, "docs", "M10-08-证据.md"),
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--results":
        options.results = path.resolve(repoRoot, next());
        break;
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--help":
      case "-h":
        console.log("用法：node scripts/test/m10-08/aggregate-m10-08.mjs [--results DIR] [--out FILE]");
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  return options;
}

function loadRuns(resultsDir, label) {
  const runs = [];
  for (let index = 1; index <= 3; index += 1) {
    const file = path.join(resultsDir, `metrics-run${index}-${label}.json`);
    if (!existsSync(file)) throw new Error(`缺少 ${path.relative(repoRoot, file)}`);
    runs.push(readJson(file));
  }
  return runs;
}

function medians(runs, selector) {
  const values = runs.map(selector).filter((value) => Number.isFinite(value));
  const stats = summarize(values);
  return { values, p50: stats.p50, p95: stats.p95, max: stats.max };
}

/** 兼容旧 metrics：`failures` 缺失时从 violations 中提取真实失败（不含阈值越线）。 */
function runFailures(run) {
  if (Array.isArray(run.failures)) return run.failures;
  return (run.violations ?? []).filter(
    (value) => value.includes("失败") || value.includes("恢复失败") || value.includes("quick_check 失败"),
  );
}

function lookupOptimizationAdr() {
  const dir = path.join(repoRoot, "docs", "adr");
  if (!existsSync(dir)) return null;
  const file = readdirSync(dir)
    .filter((name) => /^ADR-039-.+\.md$/.test(name))
    .sort()
    .at(-1);
  return file ? `docs/adr/${file}` : null;
}

const PATH_KEYS = new Set([
  "db",
  "dir",
  "shellLog",
  "daemonLog",
  "source",
  "backup",
  "retentionSource",
  "artifacts",
]);

function sanitizePathValue(value) {
  if (typeof value !== "string" || value.length === 0) return value;
  if (/^[A-Za-z]:[\\/]/.test(value) || value.startsWith("..") || value.startsWith("/")) {
    return `<external>/${path.basename(value.replaceAll("\\", "/"))}`;
  }
  return value.replaceAll("\\", "/");
}

function sanitizeSummary(value) {
  if (Array.isArray(value)) return value.map((entry) => sanitizeSummary(entry));
  if (!value || typeof value !== "object") return value;
  const result = {};
  for (const [key, entry] of Object.entries(value)) {
    result[key] = PATH_KEYS.has(key) && typeof entry === "string" ? sanitizePathValue(entry) : sanitizeSummary(entry);
  }
  return result;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const singleRuns = loadRuns(options.results, "single");
  const shardedRuns = loadRuns(options.results, "sharded");
  const startupPath = path.join(options.results, "startup-e2e.json");
  const startup = existsSync(startupPath) ? readJson(startupPath) : null;

  const writeP95 = medians(singleRuns, (run) => run.write?.latencyMs?.p95);
  const readP95 = medians(singleRuns, (run) =>
    Math.max(
      run.read?.samples?.eventsPage?.p95 ?? 0,
      run.read?.samples?.messagesPage?.p95 ?? 0,
      run.read?.samples?.lastSeq?.p95 ?? 0,
    ),
  );
  const quickCheckCold = medians(singleRuns, (run) => run.quickCheck?.coldMs);
  const backupMs = medians(singleRuns, (run) => run.backupRestore?.backupMs);
  const restoreMs = medians(singleRuns, (run) => run.backupRestore?.restoreMs);
  const failures = singleRuns.reduce((sum, run) => sum + runFailures(run).length, 0);
  const dataset = singleRuns[0].dataset;
  const retentionRun = singleRuns.find((run) => run.retention && !run.retention.skipped)?.retention ?? null;
  const projectedBytes = retentionRun?.projectedRetainedBytes ?? dataset.bytes?.total ?? 0;

  const sharded = {
    writeP95: medians(shardedRuns, (run) => run.write?.latencyMs?.p95),
    readP95: medians(shardedRuns, (run) =>
      Math.max(
        run.read?.samples?.eventsPage?.p95 ?? 0,
        run.read?.samples?.messagesPage?.p95 ?? 0,
        run.read?.samples?.lastSeq?.p95 ?? 0,
      ),
    ),
    quickCheckCold: medians(shardedRuns, (run) => run.quickCheck?.coldMs),
    backupMs: medians(shardedRuns, (run) => run.backupRestore?.backupMs),
    restoreMs: medians(shardedRuns, (run) => run.backupRestore?.restoreMs),
    shards: shardedRuns[0].config?.shards ?? 0,
  };

  const checks = [
    {
      metric: "单库写 P95",
      single: writeP95.p50,
      unit: "ms",
      threshold: `≤${THRESHOLDS.writeP95Ms}`,
      pass: writeP95.p50 <= THRESHOLDS.writeP95Ms,
      sharded: sharded.writeP95.p50,
    },
    {
      metric: "补读 P95",
      single: readP95.p50,
      unit: "ms",
      threshold: `<${THRESHOLDS.readP95Ms}`,
      pass: readP95.p50 < THRESHOLDS.readP95Ms,
      sharded: sharded.readP95.p50,
    },
    {
      metric: "启动 quick_check（冷）",
      single: quickCheckCold.p50,
      unit: "ms",
      threshold: `≤${THRESHOLDS.quickCheckMs}`,
      pass: quickCheckCold.p50 <= THRESHOLDS.quickCheckMs,
      sharded: sharded.quickCheckCold.p50,
    },
    {
      metric: "备份耗时",
      single: backupMs.p50,
      unit: "ms",
      threshold: `≤${THRESHOLDS.backupMs}`,
      pass: backupMs.p50 <= THRESHOLDS.backupMs,
      sharded: sharded.backupMs.p50,
    },
    {
      metric: "恢复耗时",
      single: restoreMs.p50,
      unit: "ms",
      threshold: `≤${THRESHOLDS.restoreMs}`,
      pass: restoreMs.p50 <= THRESHOLDS.restoreMs,
      sharded: sharded.restoreMs.p50,
    },
    {
      metric: "12 个月投影 / 保留后",
      single: projectedBytes,
      unit: "bytes",
      threshold: `<${THRESHOLDS.databaseBytes}（或保留策略可压到）`,
      pass: projectedBytes < THRESHOLDS.databaseBytes,
      sharded: null,
    },
    {
      metric: "失败",
      single: failures,
      unit: "次",
      threshold: "0",
      pass: failures === 0,
      sharded: null,
    },
  ];

  const shardingRequired = checks.some((check) => !check.pass);
  const decision = shardingRequired ? "sharding_required" : "retain_single";
  const optimizationAdr = lookupOptimizationAdr();
  const startupExceeds = startup?.exceedsBudget === true;
  const startupRequiresAdr = startupExceeds && !optimizationAdr;

  const improvement = {
    writeP95: roundDelta(writeP95.p50, sharded.writeP95.p50),
    readP95: roundDelta(readP95.p50, sharded.readP95.p50),
    quickCheckCold: roundDelta(quickCheckCold.p50, sharded.quickCheckCold.p50),
    backup: roundDelta(backupMs.p50, sharded.backupMs.p50),
    restore: roundDelta(restoreMs.p50, sharded.restoreMs.p50),
  };
  const improved =
    (improvement.writeP95 !== null && improvement.writeP95 < -5) ||
    (improvement.restore !== null && improvement.restore < -5);

  const summary = {
    task: "M10-08",
    adr: "ADR-038",
    generatedAt: new Date().toISOString(),
    dataset: {
      rows: dataset.rows,
      bytes: dataset.bytes,
      sha256: dataset.sha256,
      sessions: dataset.sessions,
      typeDistribution: dataset.typeDistribution,
    },
    machine: singleRuns[0].machine,
    runs: {
      single: singleRuns.map((run) => ({
        runIndex: run.runIndex,
        writeP95Ms: run.write?.latencyMs?.p95,
        writeMaxMs: run.write?.latencyMs?.max,
        events: run.write?.totalEvents,
        eventsPerSecond: run.write?.eventsPerSecond,
        readP95Ms: run.read?.samples?.eventsPage?.p95,
        readMessagesP95Ms: run.read?.samples?.messagesPage?.p95,
        readLastSeqP95Ms: run.read?.samples?.lastSeq?.p95,
        quickCheckColdMs: run.quickCheck?.coldMs,
        quickCheckMedianMs: run.quickCheck?.medianMs,
        backupMs: run.backupRestore?.backupMs,
        restoreMs: run.backupRestore?.restoreMs,
        retention: run.retention?.skipped ? null : run.retention,
        violations: run.violations,
        failures: runFailures(run),
      })),
      sharded: shardedRuns.map((run) => ({
        runIndex: run.runIndex,
        shards: run.config?.shards,
        writeP95Ms: run.write?.latencyMs?.p95,
        readP95Ms: run.read?.samples?.eventsPage?.p95,
        quickCheckColdMs: run.quickCheck?.coldMs,
        backupMs: run.backupRestore?.backupMs,
        restoreMs: run.backupRestore?.restoreMs,
        violations: run.violations,
      })),
    },
    aggregates: {
      writeP95Ms: writeP95,
      readP95Ms: readP95,
      quickCheckColdMs: quickCheckCold,
      backupMs,
      restoreMs,
      failures,
      projectedRetainedBytes: projectedBytes,
      projectedRetainedGiB: Number((projectedBytes / GIB).toFixed(3)),
    },
    sharded,
    shardedComparison: { improvement, improved, note: improved ? "分片改善" : "分片未改善" },
    judgement: checks,
    decision,
    startup,
    startupExceeds,
    optimizationAdr,
    startupRequiresAdr,
    ok: !shardingRequired && !startupRequiresAdr,
  };
  ensureDir(path.dirname(options.out));
  writeJson(options.out, sanitizeSummary(summary));

  const formatValue = (row) =>
    row.unit === "bytes" ? formatBytes(row.single) : `${row.single}${row.unit === "ms" ? "ms" : ""}`;
  const formatSharded = (value, unit) =>
    value === null || value === undefined ? "—" : unit === "bytes" ? formatBytes(value) : `${value}ms`;
  console.log("\nM10-08 判定表（3 次中位）：");
  console.log("| 指标 | 单库 | 阈值 | 分片 | 结论 |");
  console.log("|---|---|---|---|---|");
  for (const row of checks) {
    console.log(
      `| ${row.metric} | ${formatValue(row)} | ${row.threshold} | ${formatSharded(row.sharded, row.unit)} | ${
        row.pass ? "通过" : "越线"
      } |`,
    );
  }
  console.log(`\n结论：${decision}${startupExceeds ? "（启动 >1.5s，需优化 ADR）" : ""}`);
  console.log(`OK  ${path.relative(repoRoot, options.out)}`);
  if (startupRequiresAdr) {
    console.error("FAIL 启动 windowShownAt/uiReadyAt >1.5s，但未找到 docs/adr/ADR-039-*.md 优化 ADR 草案");
    process.exitCode = 1;
  }
}

function roundDelta(base, candidate) {
  if (!Number.isFinite(base) || !Number.isFinite(candidate) || base === 0) return null;
  return Number((((candidate - base) / base) * 100).toFixed(2));
}

try {
  main();
} catch (error) {
  console.error(`[m10-08-aggregate] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
}
