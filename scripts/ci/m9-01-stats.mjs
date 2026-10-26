#!/usr/bin/env node
/**
 * M9-01 DoD-3：统计脚本固化（P2-2 归档）。
 *
 * 统计口径（一律从 HEAD 源码现算，不读缓存、不读生成物注释里写的数字）：
 * - `commands`：`collect_commands![...]` 的命令数 + daemon/shell、可达/保留拆分；
 * - `frontend.invoke_call_sites`：`apps/desktop/src` 下非测试 `.ts/.tsx` 中
 *   `invoke(` / `invoke<T>(` 的调用点总数（按匹配次数计），`invoke_files` 为命中的文件数；
 * - `frontend.test_files`：`apps/desktop/src` 下 `*.test.ts(x)`；
 * - `protocol.bindings_lines`：`packages/protocol/src/bindings.ts` 行数（含空行；
 *   DoD 参照值 1003，不一致时 `matches_dod_reference=false`，以实测为准）；
 * - `core.lines`：`crates/aether-core/src` 下全部 `.rs` 的总行数；
 * - `core.rust_test_files`：`crates/<crate>/tests` 下全部 `.rs` 文件数。
 *
 * 用法：
 *   node scripts/ci/m9-01-stats.mjs           # 打印 + 写入 docs/M9-01-baseline/contract/stats.json
 *   node scripts/ci/m9-01-stats.mjs --check   # 只对比已归档值，不写文件（漂移退出码 1）
 */
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
export const STATS_TARGET = "docs/M9-01-baseline/contract/stats.json";
const DOD_BINDINGS_LINES = 1003;
const SHELL_COMMANDS = ["app_exit", "app_restart", "ref_pick", "startup_pick_target"];
const RESERVED_COMMANDS = ["startup_get", "startup_migrate"];

function abs(rel) {
  return path.join(root, rel);
}

function countLines(text) {
  if (text.length === 0) return 0;
  return text.endsWith("\n") ? text.split("\n").length - 1 : text.split("\n").length;
}

function listFiles(relDir, predicate) {
  return readdirSync(abs(relDir), { recursive: true })
    .map((entry) => String(entry).split(path.sep).join("/"))
    .filter(predicate);
}

function commandNames() {
  const source = readFileSync(abs("crates/aether-tauri/src/ipc/commands.rs"), "utf8");
  const block = source.match(/tauri_specta::collect_commands!\[([\s\S]*?)\n {4}\]/);
  if (!block) throw new Error("[m9-01-stats] 无法解析 collect_commands!");
  return block[1]
    .split(/\r?\n/)
    .map((line) => line.trim().replace(/,$/, ""))
    .filter(Boolean);
}

/** 现算统计（录制与人工复核共用）。 */
export function collectStats() {
  const commands = commandNames();
  const shell = commands.filter((name) => SHELL_COMMANDS.includes(name)).length;
  const reserved = commands.filter((name) => RESERVED_COMMANDS.includes(name)).length;

  const frontendFiles = listFiles("apps/desktop/src", (rel) => /\.(ts|tsx)$/.test(rel));
  const frontendSource = frontendFiles.filter((rel) => !/\.test\.(ts|tsx)$/.test(rel));
  let invokeCallSites = 0;
  const invokeFiles = new Set();
  for (const rel of frontendSource) {
    const text = readFileSync(abs(`apps/desktop/src/${rel}`), "utf8");
    const matches = text.match(/\binvoke\s*(?:<[^>]*>)?\s*\(/g) ?? [];
    if (matches.length > 0) {
      invokeCallSites += matches.length;
      invokeFiles.add(rel);
    }
  }

  const bindingsText = readFileSync(abs("packages/protocol/src/bindings.ts"), "utf8");
  const bindingsLines = countLines(bindingsText);

  const coreFiles = listFiles("crates/aether-core/src", (rel) => rel.endsWith(".rs"));
  const coreLines = coreFiles.reduce(
    (sum, rel) => sum + countLines(readFileSync(abs(`crates/aether-core/src/${rel}`), "utf8")),
    0,
  );

  const rustTestFiles = listFiles("crates", (rel) => /(^|\/)tests\/.*\.rs$/.test(rel)).length;

  return {
    schema: "aether.m9-01.stats.v1",
    unit_note: "行数按「以 \\n 分隔的行数」计（含空行，末尾换行不额外计一行）；调用点按正则匹配次数计。",
    commands: {
      total: commands.length,
      daemon: commands.length - shell,
      shell,
      reachable: commands.length - reserved,
      reserved,
    },
    frontend: {
      source_files: frontendSource.length,
      test_files: frontendFiles.filter((rel) => /\.test\.(ts|tsx)$/.test(rel)).length,
      invoke_call_sites: invokeCallSites,
      invoke_files: invokeFiles.size,
    },
    protocol: {
      bindings_lines: bindingsLines,
      dod_reference_bindings_lines: DOD_BINDINGS_LINES,
      matches_dod_reference: bindingsLines === DOD_BINDINGS_LINES,
    },
    core: {
      lines: coreLines,
      files: coreFiles.length,
      rust_test_files: rustTestFiles,
    },
    method: {
      commands: "crates/aether-tauri/src/ipc/commands.rs › tauri_specta::collect_commands![...]",
      invoke: "apps/desktop/src/**/*.{ts,tsx}（排除 *.test.ts(x)）› /\\binvoke\\s*(?:<[^>]*>)?\\s*\\(/g",
      frontend_tests: "apps/desktop/src/**/*.test.{ts,tsx}",
      bindings: "packages/protocol/src/bindings.ts",
      core: "crates/aether-core/src/**/*.rs",
      rust_tests: "crates/*/tests/**/*.rs",
    },
  };
}

function diffAgainstRecorded(stats) {
  let recorded;
  try {
    recorded = JSON.parse(readFileSync(abs(STATS_TARGET), "utf8"));
  } catch {
    return [`未找到已归档统计（${STATS_TARGET}）`];
  }
  const flatten = (value, prefix = "") =>
    Object.entries(value).flatMap(([key, child]) => {
      if (child && typeof child === "object" && !Array.isArray(child)) {
        return flatten(child, `${prefix}${key}.`);
      }
      return [[`${prefix}${key}`, child]];
    });
  const current = new Map(flatten(stats));
  const previous = new Map(flatten(recorded));
  const diffs = [];
  for (const [key, value] of current) {
    if (key === "generated_at" || key === "head_commit") continue;
    if (!previous.has(key)) diffs.push(`${key} 新增：${JSON.stringify(value)}`);
    else if (JSON.stringify(previous.get(key)) !== JSON.stringify(value)) {
      diffs.push(`${key}：归档 ${JSON.stringify(previous.get(key))} → 现算 ${JSON.stringify(value)}`);
    }
  }
  for (const key of previous.keys()) {
    if (!current.has(key) && key !== "generated_at" && key !== "head_commit") {
      diffs.push(`${key} 已消失（归档 ${JSON.stringify(previous.get(key))}）`);
    }
  }
  return diffs;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const stats = collectStats();
  const check = process.argv.includes("--check");
  if (check) {
    const diffs = diffAgainstRecorded(stats);
    if (diffs.length > 0) {
      console.error("[m9-01-stats] 与归档统计不一致：");
      for (const line of diffs) console.error(`  - ${line}`);
      process.exit(1);
    }
    console.log("[m9-01-stats] 与归档统计一致。");
    process.exit(0);
  }
  writeFileSync(abs(STATS_TARGET), `${JSON.stringify(stats, null, 2)}\n`, "utf8");
  console.log(
    `[m9-01-stats] 命令 ${stats.commands.total}（daemon ${stats.commands.daemon} / shell ${stats.commands.shell}，` +
      `可达 ${stats.commands.reachable} + 保留 ${stats.commands.reserved}）；` +
      `invoke ${stats.frontend.invoke_call_sites} 处/${stats.frontend.invoke_files} 文件；` +
      `前端测试 ${stats.frontend.test_files}；bindings ${stats.protocol.bindings_lines} 行；` +
      `核心 ${stats.core.lines} 行；Rust 测试文件 ${stats.core.rust_test_files}`,
  );
  console.log(`[m9-01-stats] 已写入 ${STATS_TARGET}`);
  if (!stats.protocol.matches_dod_reference) {
    console.warn(
      `[m9-01-stats] 注意：bindings.ts 实测 ${stats.protocol.bindings_lines} 行，` +
        `DoD 参照值 ${stats.protocol.dod_reference_bindings_lines} 行（以实测为准）。`,
    );
  }
}
