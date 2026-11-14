#!/usr/bin/env node
/**
 * `pnpm verify:m10-08` —— 事件分库预案（ADR-038）可复跑检查。
 *
 * 检查面：
 *  1. ADR-038 / 脚本 / 证据文档存在；
 *  2. 证据指标：数据集（≥20M 行 / 接近 5GiB / sha256 / 类型分布）、3 次单库 +
 *     3 次分片、P95/quick_check/备份/恢复/失败计数；
 *  3. 二选一判定：全部阈值内 → `retain_single`；任一越线 → `sharding_required`
 *     且必须存在新 ADR（本任务不实施分库）；
 *  4. M10-06 启动补充口径：windowShownAt/uiReadyAt >1.5s → 必须存在优化 ADR 草案；
 *  5. 契约 0 新增：迁移仍 13、命令 87/83/4、业务码 35、设置键 11、
 *     无 `events_*` 分库表/迁移；
 *  6. 可执行冒烟：小数据集构造 + 3s 写入 + 补读 + quick_check（默认开启，
 *     `AETHER_M10_08_VERIFY_SMOKE=0` 关闭）。
 *
 * 说明：本任务只验证、不实施分库；`verify` 不替代 3×10min 全量压测，
 * 全量由 `node scripts/test/m10-08/run-full.mjs` 执行。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { DATASET_DEFAULTS, GIB, repoRootFrom } from "./lib.mjs";

const root = repoRootFrom(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const checks = [];
const failures = [];

function check(name, condition, detail = "") {
  checks.push(name);
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function mustContain(rel, needles) {
  const text = read(rel);
  for (const needle of needles) {
    check(`${rel} 含 ${needle}`, text.includes(needle));
  }
}

function run(label, command, args, { allowFailure = false } = {}) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    execFileSync(command, args, {
      cwd: root,
      stdio: "inherit",
      env: process.env,
    });
    checks.push(label);
  } catch (error) {
    checks.push(label);
    if (!allowFailure) failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

// ---------- 1) ADR / 脚本 / 证据存在 ----------
mustContain("docs/adr/ADR-038-m10-event-sharding-preplan.md", [
  "已接受",
  "5 GiB",
  "20,000,000",
  "≤50ms",
  "<150ms",
  "≤30s",
  "≤600s",
  "≤900s",
  "保留单库",
  "需分库",
  "本任务不实施分库",
]);
for (const script of [
  "lib.mjs",
  "gen-events-dataset.mjs",
  "bench-events.mjs",
  "bench-worker.mjs",
  "build-shards.mjs",
  "startup-e2e.mjs",
  "aggregate-m10-08.mjs",
  "run-full.mjs",
  "parallel-quick-check.mjs",
]) {
  check(`脚本存在：scripts/test/m10-08/${script}`, existsSync(path.join(root, "scripts/test/m10-08", script)));
}
check("证据文档存在", existsSync(path.join(root, "docs/M10-08-证据.md")));
check("指标汇总存在", existsSync(path.join(root, "docs/M10-08-metrics.json")));
if (existsSync(path.join(root, "docs/M10-08-证据.md"))) {
  mustContain("docs/M10-08-证据.md", [
    "DoD-1",
    "DoD-2",
    "DoD-3",
    "DoD-4",
    "DoD-5",
    "数据集",
    "机器信息",
    "quick_check",
    "windowShownAt",
    "uiReadyAt",
    "判定表",
    "复跑",
    "未决风险",
    "M10-08-metrics.json",
  ]);
}

// ---------- 2) 指标汇总与二选一判定 ----------
if (existsSync(path.join(root, "docs/M10-08-metrics.json"))) {
  const metrics = readJson("docs/M10-08-metrics.json");
  check("decision ∈ {retain_single, sharding_required}", ["retain_single", "sharding_required"].includes(metrics.decision), String(metrics.decision));
  check("单库运行 = 3 次", metrics.runs?.single?.length === 3, String(metrics.runs?.single?.length));
  check("分片运行 = 3 次", metrics.runs?.sharded?.length === 3, String(metrics.runs?.sharded?.length));
  check("数据集 ≥ 20,000,000 行", (metrics.dataset?.rows ?? 0) >= DATASET_DEFAULTS.minRows, String(metrics.dataset?.rows));
  check(
    "数据集接近 5 GiB（4.5–5.5 GiB）",
    (metrics.dataset?.bytes?.total ?? 0) >= 4.5 * GIB && (metrics.dataset?.bytes?.total ?? 0) <= 5.5 * GIB,
    String(metrics.dataset?.bytes?.total),
  );
  check("dataset sha256 为 64 位十六进制", /^[0-9a-f]{64}$/.test(metrics.dataset?.sha256 ?? ""));
  check(
    "类型分布含 message.delta/usage/log",
    ["message.delta", "usage", "log"].every((type) => (metrics.dataset?.typeDistribution?.[type]?.rows ?? 0) > 0),
  );
  const judgement = metrics.judgement ?? [];
  check("判定表 7 行", judgement.length === 7, String(judgement.length));
  const allPass = judgement.every((row) => row.pass === true);
  if (metrics.decision === "retain_single") {
    check("retain_single 判定表全部通过", allPass);
    check("保留单库：12 个月投影 <5GiB", judgement.find((row) => row.metric.includes("12 个月"))?.pass === true);
  } else {
    check("sharding_required 至少一项越线", judgement.some((row) => row.pass === false));
    const adrDir = path.join(root, "docs", "adr");
    const shardingAdr = existsSync(adrDir)
      ? readdirSync(adrDir).filter((name) => /^ADR-0(39|40)-.+\.md$/.test(name))
      : [];
    check("需分库时必须另立 ADR", shardingAdr.length > 0, shardingAdr.join(","));
  }
  const singleRuns = metrics.runs?.single ?? [];
  for (const [index, run] of singleRuns.entries()) {
    check(`run${index + 1} 写 P95 已记录`, Number.isFinite(run.writeP95Ms), String(run.writeP95Ms));
    check(`run${index + 1} 补读 P95 已记录`, Number.isFinite(run.readP95Ms), String(run.readP95Ms));
    check(`run${index + 1} quick_check 冷启动已记录`, Number.isFinite(run.quickCheckColdMs), String(run.quickCheckColdMs));
    check(`run${index + 1} 备份/恢复已记录`, Number.isFinite(run.backupMs) && Number.isFinite(run.restoreMs));
    check(`run${index + 1} 失败 0`, (run.failures ?? []).length === 0, JSON.stringify(run.failures));
  }
  if (metrics.startupExceeds) {
    check("启动 >1.5s 时必须附优化 ADR", Boolean(metrics.optimizationAdr), String(metrics.optimizationAdr));
    check("优化 ADR 文件存在", Boolean(metrics.optimizationAdr) && existsSync(path.join(root, metrics.optimizationAdr)));
  }
  check(
    "保留策略清理记录（30 天）",
    singleRuns.some((run) => run.retention && run.retention.retentionDays === 30),
  );
  check(
    "分片对照改善结论已记录",
    ["分片改善", "分片未改善"].includes(metrics.shardedComparison?.note),
    String(metrics.shardedComparison?.note),
  );
  check(
    "失败阈值 0",
    judgement.find((row) => row.metric === "失败")?.single === 0,
    String(judgement.find((row) => row.metric === "失败")?.single),
  );
}

// ---------- 3) 契约 0 新增 ----------
const migrations = readdirSync(path.join(root, "migrations")).filter((name) => /^\d{4}_.+\.sql$/.test(name));
const expectedMigrations = [
  "0001_init.sql",
  "0002_unique_keys.sql",
  "0003_p0_ui_extensions.sql",
  "0004_permission_mode_and_rules.sql",
  "0005_audit_indexes.sql",
  "0006_runtime_enabled.sql",
  "0007_presets.sql",
  "0008_session_search_fts.sql",
  "0009_provider_endpoints.sql",
  "0010_workflow_orchestration.sql",
  "0011_subagent_results.sql",
  "0012_attachments.sql",
  "0013_scheduler_queue.sql",
];
check(
  "M10-08 未新增迁移（0001–0013 保持）",
  expectedMigrations.every((name) => migrations.includes(name)),
  migrations.join(","),
);
check(
  "迁移文件连续（0001 起）",
  migrations
    .slice()
    .sort()
    .every((name, index) => Number(name.slice(0, 4)) === index + 1),
  migrations.join(","),
);
const migrationText = migrations.map((name) => read(`migrations/${name}`)).join("\n");
check("迁移无 events_* 分库表", !/CREATE\s+TABLE\s+events_/i.test(migrationText));
const manifest = readJson("packages/protocol/src/command-manifest.json");
check("命令 87（0 新增）", manifest.counts?.total === 87, String(manifest.counts?.total));
check("daemon 83 / shell 4", manifest.counts?.daemon === 83 && manifest.counts?.shell === 4);
const errorCodes = readJson("packages/protocol/src/error_codes.json");
check("业务码 35（0 新增）", errorCodes.business?.length === 35, String(errorCodes.business?.length));
const dto = read("crates/aether-app/src/ipc/dto.rs");
check("设置键仍 11", /SETTINGS_KEY_ALLOWLIST[\s\S]*?"ui.theme",\s*\];/.test(dto));

// ---------- 4) 可执行冒烟 ----------
const smokeEnabled = process.env.AETHER_M10_08_VERIFY_SMOKE !== "0";
if (smokeEnabled) {
  const smokeRoot = path.join(root, "scripts", "test", ".tmp", "m10-08-verify");
  const smokeDataset = path.join(smokeRoot, "dataset");
  const smokeResults = path.join(smokeRoot, "results");
  rmSync(smokeRoot, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  console.log("\n[m10-08-verify] 冒烟数据集构造（128 MiB / 500k rows / 10 sessions）");
  run("冒烟数据集构造", process.execPath, [
    path.join(here, "gen-events-dataset.mjs"),
    "--out",
    smokeDataset,
    "--target-mib",
    "128",
    "--min-rows",
    "500000",
    "--sessions",
    "10",
    "--quiet",
  ]);
  run("冒烟压测（3s 写入 + 2s 补读 + quick_check）", process.execPath, [
    path.join(here, "bench-events.mjs"),
    "--dataset",
    smokeDataset,
    "--out",
    smokeResults,
    "--run-index",
    "1",
    "--label",
    "single",
    "--write-seconds",
    "3",
    "--read-seconds",
    "2",
    "--writers",
    "4",
    "--rate",
    "10",
    "--quick-check-runs",
    "1",
    "--skip-backup",
    "--skip-retention",
    "--quiet",
  ]);
  const smokeMetricsPath = path.join(smokeResults, "metrics-run1-single.json");
  check("冒烟指标文件生成", existsSync(smokeMetricsPath));
  if (existsSync(smokeMetricsPath)) {
    const smoke = JSON.parse(readFileSync(smokeMetricsPath, "utf8"));
    check("冒烟写入事件 >0", (smoke.write?.totalEvents ?? 0) > 0, String(smoke.write?.totalEvents));
    check("冒烟补读样本 >0", (smoke.read?.samples?.eventsPage?.count ?? 0) > 0);
    check("冒烟 quick_check ok", smoke.quickCheck?.ok === true);
  }
  if (!process.env.AETHER_M10_08_KEEP_SMOKE) {
    rmSync(smokeRoot, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
}

// ---------- 5) 产物/生成物 diff ----------
run("command-manifest diff=0", process.execPath, ["scripts/ci/command-manifest.mjs", "check"]);
run("error-codes diff=0", process.execPath, ["scripts/ci/error-codes.mjs", "check"]);
run("protocol-client diff=0", process.execPath, ["scripts/ci/protocol-client.mjs", "check"]);
run("M9-01 基线", process.execPath, ["scripts/test/verify-m9-01-baseline.mjs"]);

const passed = checks.length - failures.length;
if (failures.length > 0) {
  console.error(`\n[M10-08] FAIL：${failures.length} 项失败`);
  for (const failure of failures) console.error(` - ${failure}`);
  process.exit(1);
}
console.log(
  `\n[m10-08] OK：${passed}/${checks.length} 项通过（ADR-038；只验证不分库；` +
    `5GiB/20M 行；3×10min 单库 + 3×10min 分片；判定二选一）`,
);
