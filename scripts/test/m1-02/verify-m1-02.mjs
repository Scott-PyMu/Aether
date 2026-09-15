/**
 * M1-02 验证脚本：领域模型与事件信封 v1（D4/D12/附录 B/附录 C）。
 *
 * 覆盖 DoD：
 *   1) 信封字段 ↔ events 表列双向一一对应（静态 DDL 解析 + cargo 契约/入库测试）；
 *   2) 附录 B 类型清单严格一致（设计文档 ↔ Rust 事件类型注册表）+ 全类型 payload 校验；
 *   3) 非法输入矩阵（未知字段/非法枚举/负 seq）由契约测试拒绝；
 *   4) events 表含 v/runtime_id 列（静态 + 真实 SQLite 断言）。
 *
 * 说明：入库往返测试（aether-store）依赖 rusqlite(bundled)，需要 C 编译器；
 * 无 C 编译器的环境该检查为环境性失败（见任务报告「未解决项」）。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const node = process.execPath;
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const ENVELOPE_FIELDS = [
  "v",
  "id",
  "session_id",
  "run_id",
  "runtime_id",
  "seq",
  "ts",
  "type",
  "payload",
];

// ===== 静态断言：迁移 0001 的 events 列集合 =====

function stripSqlComments(sql) {
  return sql
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, ""))
    .join("\n");
}

function createTableBody(sql, table) {
  const marker = `CREATE TABLE ${table} (`;
  const start = sql.indexOf(marker);
  if (start < 0) throw new Error(`迁移缺少表 ${table}`);
  const bodyStart = start + marker.length;
  let depth = 1;
  for (let i = bodyStart; i < sql.length; i += 1) {
    const ch = sql[i];
    if (ch === "(") depth += 1;
    else if (ch === ")") {
      depth -= 1;
      if (depth === 0) return sql.slice(bodyStart, i);
    }
  }
  throw new Error(`表 ${table} 定义未闭合`);
}

function splitTopLevel(body) {
  const parts = [];
  let depth = 0;
  let current = "";
  for (const ch of body) {
    if (ch === "(") {
      depth += 1;
      current += ch;
    } else if (ch === ")") {
      depth -= 1;
      current += ch;
    } else if (ch === "," && depth === 0) {
      parts.push(current);
      current = "";
    } else {
      current += ch;
    }
  }
  parts.push(current);
  return parts;
}

function tableColumns(sql, table) {
  const constraints = new Set(["CHECK", "PRIMARY", "UNIQUE", "FOREIGN", "CONSTRAINT"]);
  return splitTopLevel(createTableBody(stripSqlComments(sql), table))
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => part.split(/\s+/)[0])
    .filter((name) => !constraints.has(name));
}

function checkMigrationStatic() {
  const migrationPath = path.join(repoRoot, "migrations", "0001_init.sql");
  const sql = readFileSync(migrationPath, "utf8");
  const columns = tableColumns(sql, "events");
  const columnSet = new Set(columns);
  const fieldSet = new Set(ENVELOPE_FIELDS);

  if (columns.length !== ENVELOPE_FIELDS.length) {
    throw new Error(`events 列数 ${columns.length} != 信封字段数 ${ENVELOPE_FIELDS.length}`);
  }
  for (const column of columns) {
    if (!fieldSet.has(column)) throw new Error(`表列 ${column} 不在信封字段中`);
  }
  for (const field of ENVELOPE_FIELDS) {
    if (!columnSet.has(field)) throw new Error(`信封字段 ${field} 不在 events 表列中`);
  }
  for (const required of ["v", "runtime_id"]) {
    if (!columnSet.has(required)) throw new Error(`events 缺少 ${required} 列（DoD4）`);
  }
  console.log(
    `[static] events 列集合 == 信封字段集合（${columns.length} 列）: ${columns.join(", ")}`,
  );
}

// ===== 静态断言：附录 B 类型清单 ↔ Rust 事件类型注册表 =====

function expandTypeCell(cell) {
  const token = cell.replace(/`/g, "").trim();
  const parts = token.split("/");
  if (parts.length === 1) return parts;
  const first = parts[0];
  const cut = Math.max(first.lastIndexOf("."), first.lastIndexOf("_")) + 1;
  const base = first.slice(0, cut);
  return parts.map((part, index) => (index === 0 ? first : `${base}${part}`));
}

function appendixBTypes() {
  const doc = readFileSync(path.join(repoRoot, "设计文档.md"), "utf8");
  const start = doc.indexOf("## 附录 B");
  const end = doc.indexOf("## 附录 C");
  if (start < 0 || end < 0) throw new Error("设计文档缺少附录 B/C");
  const section = doc.slice(start, end);
  const all = new Set();
  const mvp = new Set();
  const reserved = new Set();
  const seen = [];
  for (const line of section.split(/\r?\n/)) {
    if (!line.trim().startsWith("|")) continue;
    const cells = line.split("|").map((cell) => cell.trim());
    if (cells.length < 5) continue;
    const typeCell = cells[1];
    const mvpCell = cells[2];
    if (!typeCell.includes("`")) continue;
    for (const type of expandTypeCell(typeCell)) {
      if (all.has(type)) throw new Error(`附录 B 类型重复：${type}`);
      all.add(type);
      seen.push(`${type}(${mvpCell.includes("✅") ? "MVP" : "预留"})`);
      if (mvpCell.includes("✅")) mvp.add(type);
      else reserved.add(type);
    }
  }
  return { all, mvp, reserved, seen };
}

function rustEventTypes() {
  const source = readFileSync(path.join(repoRoot, "crates", "aether-core", "src", "event.rs"), "utf8");
  const asStrStart = source.indexOf("pub const fn as_str(self)");
  if (asStrStart < 0) throw new Error("event.rs 缺少 as_str");
  const asStrEnd = source.indexOf("\n    }", asStrStart);
  const asStrBlock = source.slice(asStrStart, asStrEnd);
  const byVariant = new Map();
  for (const match of asStrBlock.matchAll(/Self::([A-Za-z0-9_]+)\s*=>\s*"([^"]+)"/g)) {
    byVariant.set(match[1], match[2]);
  }
  if (byVariant.size === 0) throw new Error("event.rs as_str 未解析到类型");

  const constArray = (name) => {
    const marker = `pub const ${name}: [Self;`;
    const start = source.indexOf(marker);
    if (start < 0) throw new Error(`event.rs 缺少 ${name}`);
    const end = source.indexOf("];", start);
    const block = source.slice(start, end);
    return [...block.matchAll(/Self::([A-Za-z0-9_]+)/g)].map((match) => byVariant.get(match[1]));
  };
  return { all: new Set(byVariant.values()), mvp: new Set(constArray("MVP")), reserved: new Set(constArray("RESERVED")) };
}

function checkSetsEqual(label, actual, expected) {
  const missing = [...expected].filter((item) => !actual.has(item));
  const extra = [...actual].filter((item) => !expected.has(item));
  if (missing.length > 0 || extra.length > 0) {
    throw new Error(`${label} 不一致：缺失=${JSON.stringify(missing)} 多出=${JSON.stringify(extra)}`);
  }
}

function checkAppendixBStatic() {
  const doc = appendixBTypes();
  const rust = rustEventTypes();
  checkSetsEqual("附录 B 全部类型 ↔ Rust 类型", rust.all, doc.all);
  checkSetsEqual("附录 B MVP 集 ↔ Rust MVP", rust.mvp, doc.mvp);
  checkSetsEqual("附录 B 预留类型 ↔ Rust RESERVED", rust.reserved, doc.reserved);
  console.log(`[static] 附录 B 类型清单一致（${doc.all.size} 种）: ${doc.seen.join(", ")}`);
}

// ===== 执行 =====

const staticChecks = [
  ["静态：events 列 ↔ 信封字段（含 v/runtime_id）", checkMigrationStatic],
  ["静态：附录 B 类型清单 ↔ Rust 注册表（MVP/预留）", checkAppendixBStatic],
];
for (const [name, check] of staticChecks) {
  try {
    check();
    record(name, 0);
  } catch (error) {
    console.error(`[static] 失败：${error.message}`);
    record(name, 1);
  }
}

record(
  "cargo test -p aether-core（DoD1 字段/DoD2 payload/DoD3 非法矩阵）",
  run(cargo, ["test", "-p", "aether-core"]),
);
record(
  "cargo test -p aether-store --test m1_02_events_roundtrip（DoD1 真实入库/DoD4）",
  run(cargo, [
    "test",
    "-p",
    "aether-store",
    "--test",
    "m1_02_events_roundtrip",
  ]),
);

process.exit(summarize("verify-m1-02", checks));
