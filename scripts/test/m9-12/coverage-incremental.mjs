#!/usr/bin/env node
/**
 * M9-12：增量覆盖率报告 + Tauri 删除文件清单（ADR-029 §8.2 P2-7）。
 *
 * 背景：M9-02～G8 期间 `aether-tauri` 从覆盖率门禁排除；被排除窗口必须用
 * `aether-app`/`aether-rpc`/`aether-daemon` 的**增量覆盖数字**与 **Tauri 删除文件
 * 清单**替代，G8 恢复全量时再给出前后对比。本脚本不改变门禁，只归档证据。
 *
 * 用法：
 *   node scripts/test/m9-12/coverage-incremental.mjs --report <llvm-cov.json>
 *   node scripts/test/m9-12/coverage-incremental.mjs --static   # 只生成删除清单
 *
 * 报告来源（CI）：
 *   cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70
 *   cargo llvm-cov report --workspace --exclude aether-tauri --json --summary-only \
 *     --output-path scripts/test/.tmp/m9-12/coverage/llvm-cov.json
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-12", "coverage");
const incrementalPath = path.join(outDir, "incremental.json");
const deletionPath = path.join(outDir, "tauri-deletion-list.json");
const INCREMENTAL_CRATES = ["aether-app", "aether-rpc", "aether-daemon"];

function argValue(flag) {
  const index = process.argv.indexOf(flag);
  return index >= 0 ? process.argv[index + 1] : null;
}

function walk(root, base = root, out = []) {
  if (!existsSync(root)) return out;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      walk(full, base, out);
    } else if (entry.isFile()) {
      out.push(path.relative(base, full).split(path.sep).join("/"));
    }
  }
  return out;
}

/** G8 待删除面：Tauri crate + Tauri 专用生成物（迁移期仅开发对照）。 */
function buildDeletionList() {
  const entries = [];
  const tauriDir = path.join(repoRoot, "crates", "aether-tauri");
  for (const rel of walk(tauriDir).sort()) {
    entries.push({
      path: `crates/aether-tauri/${rel}`,
      bytes: statSync(path.join(tauriDir, rel)).size,
    });
  }
  // `bindings.ts` 为 tauri-specta 迁移期生成物；M9-08 起前端不再消费，G8 一并移除。
  const bindings = path.join(repoRoot, "packages", "protocol", "src", "bindings.ts");
  if (existsSync(bindings)) {
    entries.push({
      path: "packages/protocol/src/bindings.ts",
      bytes: statSync(bindings).size,
    });
  }
  return entries;
}

function readReport(reportPath) {
  const raw = JSON.parse(readFileSync(reportPath, "utf8").replace(/^\uFEFF/, ""));
  const data = Array.isArray(raw.data) ? raw.data : [];
  const files = data.flatMap((item) => (Array.isArray(item.files) ? item.files : []));
  return files;
}

function normalizePath(filename) {
  return String(filename).replaceAll("\\", "/");
}

function crateOf(filename) {
  const value = normalizePath(filename);
  for (const crate of INCREMENTAL_CRATES) {
    if (value.includes(`/crates/${crate}/`) || value.includes(`crates/${crate}/`)) {
      return crate;
    }
  }
  return null;
}

function summarize(files, crate) {
  let covered = 0;
  let total = 0;
  for (const file of files) {
    if (crateOf(file.filename) !== crate) continue;
    const lines = file.summary?.lines ?? {};
    covered += Number(lines.covered ?? 0);
    total += Number(lines.count ?? 0);
  }
  return {
    lines_covered: covered,
    lines_total: total,
    lines_percent: total === 0 ? null : Number(((covered / total) * 100).toFixed(2)),
  };
}

mkdirSync(outDir, { recursive: true });

const deletions = buildDeletionList();
writeFileSync(
  deletionPath,
  `${JSON.stringify(
    {
      schema: "aether.m9-12.tauri-deletion-list.v1",
      generated_at: new Date().toISOString(),
      platform: process.platform,
      note: "G8 删除清单（M9-14）；M9-02～G8 覆盖率排除期以本清单核对未迁移遗漏。",
      count: deletions.length,
      entries: deletions,
    },
    null,
    2,
  )}\n`,
  "utf8",
);
console.log(`[m9-12-coverage] Tauri 删除清单：${deletions.length} 个文件 → ${path.relative(repoRoot, deletionPath)}`);

const reportPath = argValue("--report");
const staticOnly = process.argv.includes("--static");
const deletionOnly = process.argv.includes("--deletion-only");
if (deletionOnly) {
  console.log("[m9-12-coverage] 仅生成 Tauri 删除清单（保留既有增量报告）");
  process.exit(0);
}
if (staticOnly || !reportPath) {
  const summary = {
    schema: "aether.m9-12.coverage-incremental.v1",
    generated_at: new Date().toISOString(),
    platform: process.platform,
    status: "skipped",
    reason: staticOnly
      ? "static 模式（只生成 Tauri 删除清单）"
      : "未提供 --report（Windows GNU 工具链无 profiler_builtins；由 CI ubuntu 生成）",
    gate: {
      command:
        "cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70",
      fail_under_lines: 70,
      window: "M9-02～G8",
      full_restore: "G8 恢复全量并给出前后对比表（M9-13/M9-14）",
    },
    incremental: Object.fromEntries(
      INCREMENTAL_CRATES.map((crate) => [crate, { lines_percent: null, status: "skipped" }]),
    ),
    deletion_list: path.relative(repoRoot, deletionPath),
  };
  writeFileSync(incrementalPath, `${JSON.stringify(summary, null, 2)}\n`, "utf8");
  console.log(`[m9-12-coverage] SKIP 增量覆盖（${summary.reason}）`);
  process.exit(0);
}

if (!existsSync(reportPath)) {
  console.error(`[m9-12-coverage] 缺少覆盖率报告：${reportPath}`);
  process.exit(1);
}
const files = readReport(reportPath);
if (files.length === 0) {
  console.error(`[m9-12-coverage] 报告不含 files：${reportPath}`);
  process.exit(1);
}

const incremental = Object.fromEntries(
  INCREMENTAL_CRATES.map((crate) => [crate, summarize(files, crate)]),
);
const missing = INCREMENTAL_CRATES.filter(
  (crate) => incremental[crate].lines_total === 0,
);
const result = {
  schema: "aether.m9-12.coverage-incremental.v1",
  generated_at: new Date().toISOString(),
  platform: process.platform,
  status: missing.length === 0 ? "ok" : "missing-crates",
  report: path.relative(repoRoot, path.resolve(reportPath)),
  gate: {
    command: "cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70",
    fail_under_lines: 70,
    window: "M9-02～G8",
    full_restore: "G8 恢复全量并给出前后对比表（M9-13/M9-14）",
  },
  incremental,
  deletion_list: path.relative(repoRoot, deletionPath),
  deletion_count: deletions.length,
};
writeFileSync(incrementalPath, `${JSON.stringify(result, null, 2)}\n`, "utf8");
console.log(
  `[m9-12-coverage] 增量覆盖：${INCREMENTAL_CRATES.map(
    (crate) =>
      `${crate}=${incremental[crate].lines_percent ?? "n/a"}%（${incremental[crate].lines_covered}/${incremental[crate].lines_total}）`,
  ).join("；")}`,
);
console.log(`[m9-12-coverage] 报告：${path.relative(repoRoot, incrementalPath)}`);
if (missing.length > 0) {
  console.error(`[m9-12-coverage] 报告缺少 crate：${missing.join(", ")}`);
  process.exit(1);
}
