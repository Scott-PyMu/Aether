#!/usr/bin/env node
/**
 * M9-13 DoD-7：覆盖率对比（前端不下降 + Rust 增量/全量对比）。
 *
 * 口径（ADR-029 §8.2 P2-7）：
 *  - M9-02～G8：Rust 覆盖率门禁排除 `aether-tauri`；增量覆盖归档
 *    `aether-app`/`aether-rpc`/`aether-daemon` + Tauri 删除文件清单；
 *  - G8 后恢复全量：本脚本在存在 `full.json` 时给出前后对比表；否则明确
 *    `g8_full.status=pending`（不得静默跳过）。
 *
 * 输入：
 *  - `apps/desktop/coverage/coverage-summary.json`（可 `--refresh` 重跑）
 *  - `scripts/test/.tmp/m9-12/coverage/incremental.json`（M9-12 CI/本地）
 *  - `scripts/test/.tmp/m9-13/coverage/llvm-cov.json`（可选，M9-13 本机/CI）
 *  - `scripts/test/.tmp/m9-13/coverage/full.json`（可选，G8 后全量）
 *
 * 输出：`scripts/test/.tmp/m9-13/coverage/coverage.json`。
 */
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { pnpmCommand, repoRoot } from "../lib/exec.mjs";

const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-13", "coverage");
const outPath = path.join(outDir, "coverage.json");
const m912Incremental = path.join(repoRoot, "scripts", "test", ".tmp", "m9-12", "coverage", "incremental.json");
const m913Incremental = path.join(outDir, "incremental.json");
const llvmReport = path.join(outDir, "llvm-cov.json");
const fullReport = path.join(outDir, "full.json");
const frontendSummary = path.join(repoRoot, "apps", "desktop", "coverage", "coverage-summary.json");
const baseline = JSON.parse(
  readFileSync(path.join(repoRoot, "docs/M9-12-baseline/frontend-coverage.json"), "utf8"),
);
const refresh = process.argv.includes("--refresh");
const pnpm = pnpmCommand();
mkdirSync(outDir, { recursive: true });

function run(command, args, timeout = 20 * 60 * 1000) {
  console.log(`\n$ ${[command, ...args].join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    encoding: "utf8",
    env: { ...process.env, CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1" },
    timeout,
    maxBuffer: 128 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  console.log(output.trim().split(/\r?\n/).slice(-15).join("\n"));
  return { exit: result.status ?? 1, output };
}

function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

const checks = [];
function record(name, ok, detail) {
  checks.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
}

if (refresh || !existsSync(frontendSummary)) {
  const result = run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/desktop", "coverage"]);
  record("前端覆盖率生成（vitest --coverage）", result.exit === 0, `exit=${result.exit}`);
} else {
  record("前端覆盖率使用现有 summary", true, path.relative(repoRoot, frontendSummary));
}

let frontend = null;
if (existsSync(frontendSummary)) {
  const summary = readJson(frontendSummary).total;
  frontend = {
    lines_pct: summary.lines.pct,
    statements_pct: summary.statements.pct,
    branches_pct: summary.branches.pct,
    functions_pct: summary.functions.pct,
    lines_total: summary.lines.total,
    lines_covered: summary.lines.covered,
  };
  const tolerance = 0.05;
  for (const key of ["lines_pct", "statements_pct", "branches_pct", "functions_pct"]) {
    const actual = Number(frontend[key]);
    const expected = Number(baseline.total[key]);
    record(
      `前端覆盖率不下降：${key}`,
      actual + tolerance >= expected,
      `${actual}% ≥ ${expected}%（容差 ${tolerance}pp）`,
    );
  }
  record("前端行覆盖 ≥70%", frontend.lines_pct >= 70, `${frontend.lines_pct}%`);
} else {
  record("前端覆盖率 summary 存在", false, path.relative(repoRoot, frontendSummary));
}

// Rust 增量：优先 M9-13 报告；否则复用 M9-12 增量（CI 产物）。
let incremental = null;
if (existsSync(llvmReport)) {
  const result = run(process.execPath, [
    path.join(repoRoot, "scripts", "test", "m9-12", "coverage-incremental.mjs"),
    "--report",
    llvmReport,
  ]);
  record("Rust 增量覆盖报告生成（M9-13 llvm-cov）", result.exit === 0, `exit=${result.exit}`);
  if (result.exit === 0 && existsSync(m912Incremental)) {
    copyFileSync(m912Incremental, m913Incremental);
  }
}
if (existsSync(m913Incremental)) {
  incremental = readJson(m913Incremental);
  record("Rust 增量覆盖（M9-13）", true, path.relative(repoRoot, m913Incremental));
} else if (existsSync(m912Incremental)) {
  incremental = readJson(m912Incremental);
  record(
    "Rust 增量覆盖（复用 M9-12 CI 产物）",
    true,
    path.relative(repoRoot, m912Incremental),
  );
} else {
  record("Rust 增量覆盖报告存在", false, "缺少 M9-12/M9-13 incremental.json");
}

const crateLines = {};
const incrementalCrates = incremental?.incremental ?? incremental?.crates ?? {};
for (const [crate, value] of Object.entries(incrementalCrates)) {
  crateLines[crate] = value.lines_percent ?? value.lines_pct ?? value.lines?.pct ?? null;
}

// G8 后全量对比（M9-14 执行；本任务只登记 pending/对比表）。
let full = null;
if (existsSync(fullReport)) {
  const report = readJson(fullReport);
  const crates = {};
  for (const item of report.data ?? []) {
    for (const file of item.files ?? []) {
      const match = /(?:^|\/)(aether-[a-z-]+)\//.exec(String(file.filename).replaceAll("\\", "/"));
      if (!match) continue;
      const crate = match[1];
      const lines = file.summary?.lines ?? {};
      crates[crate] ??= { covered: 0, total: 0 };
      crates[crate].covered += Number(lines.covered ?? 0);
      crates[crate].total += Number(lines.count ?? 0);
    }
  }
  full = {
    source: path.relative(repoRoot, fullReport).split(path.sep).join("/"),
    crates: Object.fromEntries(
      Object.entries(crates).map(([crate, value]) => [
        crate,
        {
          lines_covered: value.covered,
          lines_total: value.total,
          lines_pct: value.total > 0 ? Math.round((value.covered / value.total) * 10000) / 100 : 0,
        },
      ]),
    ),
  };
  record("G8 全量覆盖率报告存在", true, full.source);
} else {
  const tauriRetired = !existsSync(path.join(repoRoot, "crates", "aether-tauri"));
  record(
    "G8 全量覆盖率对比",
    !tauriRetired,
    tauriRetired
      ? "FAIL：Tauri 已退役（G8），必须生成 full.json 全量覆盖率"
      : "pending：M9-13 处于 M9-02～G8 排除 aether-tauri 窗口；M9-14/G8 恢复全量后写入 full.json",
  );
}

const passed = checks.every((item) => item.ok);
const evidence = {
  schema: "aether.m9-13.coverage.v1",
  task: "M9-13",
  finished_at: new Date().toISOString(),
  platform: { os: process.platform, arch: process.arch, node: process.version },
  gate: {
    rust_command: existsSync(path.join(repoRoot, "crates", "aether-tauri"))
      ? "cargo llvm-cov --workspace --exclude aether-tauri --ignore-filename-regex='(\\.cargo/registry|/rustc/|(crates/)?aether-(app|rpc|daemon)/)' --fail-under-lines 70"
      : "cargo llvm-cov --workspace --fail-under-lines 70（G8 全量；Tauri 已退役）",
    tauri_excluded_until: existsSync(path.join(repoRoot, "crates", "aether-tauri"))
      ? "G8（M9-14）"
      : "已结束（G8/M9-14）",
    frontend_baseline: "docs/M9-12-baseline/frontend-coverage.json",
  },
  frontend: { baseline: baseline.total, actual: frontend },
  rust: {
    incremental,
    crate_lines: crateLines,
    tauri_deletion_count: existsSync(path.join(repoRoot, "crates", "aether-tauri"))
      ? (incremental?.deletion_count ?? incremental?.tauri_deletion?.count ?? null)
      : 0,
    g8_full: full ?? { status: "pending", reason: "M9-14/G8 恢复全量后执行" },
  },
  checks,
  passed,
};
writeFileSync(outPath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
console.log(`[m9-13-coverage] 证据：${path.relative(repoRoot, outPath)}`);
if (!passed) {
  console.error(`[m9-13-coverage] FAIL ${checks.filter((item) => !item.ok).length} 项`);
  process.exit(1);
}
console.log("[m9-13-coverage] PASS");
console.log("AETHER_M9_13_COVERAGE PASS");
