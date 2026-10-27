#!/usr/bin/env node
/**
 * `pnpm verify:error-codes`（M9-01 DoD-4/DoD-10；ADR-029 R-P0-2、§7、AGENTS §2.8）。
 *
 * 校验口径（M9-01 只做骨架，业务常量自 M9-03 落地）：
 *  1. diff=0：`packages/protocol/src/error_codes.json` 与生成器重新生成的结果逐字节一致
 *     （生成物禁手改）；生成物缺失/多键/少键都算失败。
 *  2. ipc=32：`ipc` 恰好 32 项，且代码集合与**顺序**都与
 *     `crates/aether-app/src/ipc/error.rs` 的 `IpcErrorCode` 声明一致
 *     （本脚本独立解析枚举，不复用生成器的解析结果，防「生成器自己和自己一致」）。
 *  3. schema/去重/kind：每项仅含 `code/kind/owner/first_adr`；`code` 为 snake_case；
 *     `kind` 与所属集合一致（`ipc` / `business`）；`owner` 形如 `M9-01`；
 *     `first_adr` 为 `ADR-nnn` 或 null；集合内与跨集合均无重复。
 *  4. business 集合：M9-01 只校验结构/去重/kind；集合与业务常量的一致性校验
 *     自 M9-03 启用（`--business-set`）。
 *
 * 用法：
 *   node scripts/test/verify-error-codes.mjs                # M9-01 口径（默认）
 *   node scripts/test/verify-error-codes.mjs --business-set # M9-03 起：额外校验业务码集合
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { buildErrorCodes, renderErrorCodes } from "../ci/error-codes.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const TARGET = "packages/protocol/src/error_codes.json";
const SOURCE = "crates/aether-app/src/ipc/error.rs";
const IPC_EXPECTED = 32;
const ENTRY_KEYS = ["code", "kind", "owner", "first_adr"];
const BUSINESS_SET_ENABLED = process.argv.includes("--business-set");

const failures = [];
const notes = [];

function fail(message) {
  failures.push(message);
}

function note(message) {
  notes.push(message);
}

/** 独立解析 `IpcErrorCode` 的声明顺序与 `as_str()` 文本（不复用生成器逻辑）。 */
function expectedIpcCodes() {
  const source = readFileSync(path.join(root, SOURCE), "utf8");
  const body = source.match(/pub enum IpcErrorCode \{([\s\S]*?)\n\}/);
  if (!body) throw new Error(`无法从 ${SOURCE} 解析 IpcErrorCode 枚举`);
  const variants = [];
  for (const line of body[1].split(/\r?\n/)) {
    const variant = line.trim().match(/^([A-Z]\w+),$/);
    if (variant) variants.push(variant[1]);
  }
  const map = new Map();
  const impl = source.match(/pub const fn as_str\(self\) -> &'static str \{([\s\S]*?)\n {4}\}/);
  if (!impl) throw new Error(`无法从 ${SOURCE} 解析 as_str()`);
  for (const arm of impl[1].matchAll(/Self::(\w+)\s*=>\s*"([a-z0-9_]+)"/g)) {
    map.set(arm[1], arm[2]);
  }
  return variants.map((name) => {
    const code = map.get(name);
    if (!code) throw new Error(`变体 ${name} 缺少 as_str() 映射`);
    return code;
  });
}

function checkEntry(entry, collection, index, seen) {
  const at = `${collection}[${index}]`;
  if (entry === null || typeof entry !== "object" || Array.isArray(entry)) {
    fail(`${at} 不是对象`);
    return;
  }
  const keys = Object.keys(entry).sort();
  if (JSON.stringify(keys) !== JSON.stringify([...ENTRY_KEYS].sort())) {
    fail(`${at} 字段集合必须是 ${ENTRY_KEYS.join("/")}，实际 ${keys.join("/") || "<空>"}`);
    return;
  }
  if (typeof entry.code !== "string" || !/^[a-z][a-z0-9_]*$/.test(entry.code)) {
    fail(`${at}.code 必须是 snake_case 字符串，实际 ${JSON.stringify(entry.code)}`);
  }
  if (entry.kind !== collection) {
    fail(`${at}.kind 必须是 "${collection}"，实际 ${JSON.stringify(entry.kind)}`);
  }
  if (typeof entry.owner !== "string" || !/^M\d{1,2}-\d{2}$/.test(entry.owner)) {
    fail(`${at}.owner 必须形如 M9-01，实际 ${JSON.stringify(entry.owner)}`);
  }
  if (entry.first_adr !== null && !/^ADR-\d{3}$/.test(String(entry.first_adr))) {
    fail(`${at}.first_adr 必须为 ADR-nnn 或 null，实际 ${JSON.stringify(entry.first_adr)}`);
  }
  if (seen.has(entry.code)) {
    fail(`${at}.code 重复：${entry.code}`);
  }
  seen.add(entry.code);
}

let parsed;
try {
  parsed = JSON.parse(readFileSync(path.join(root, TARGET), "utf8"));
} catch (error) {
  console.error(`[error-codes] 阻断：无法读取/解析 ${TARGET}：${error.message}`);
  console.error("  先运行 `node scripts/ci/error-codes.mjs generate` 并入库。");
  process.exit(1);
}

if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
  fail(`${TARGET} 顶层必须是对象`);
} else {
  const keys = Object.keys(parsed);
  if (JSON.stringify(keys) !== JSON.stringify(["ipc", "business"])) {
    fail(`顶层键必须恰好是 ["ipc","business"]，实际 ${JSON.stringify(keys)}`);
  }
}

const ipc = Array.isArray(parsed?.ipc) ? parsed.ipc : [];
const business = Array.isArray(parsed?.business) ? parsed.business : [];
if (!Array.isArray(parsed?.ipc)) fail("ipc 必须是数组");
if (!Array.isArray(parsed?.business)) fail("business 必须是数组");

const seen = new Set();
ipc.forEach((entry, index) => checkEntry(entry, "ipc", index, seen));
business.forEach((entry, index) => checkEntry(entry, "business", index, seen));

if (ipc.length !== IPC_EXPECTED) {
  fail(`ipc 条目数必须是 ${IPC_EXPECTED}（ADR-029 R-P0-2），实际 ${ipc.length}`);
}

const expected = expectedIpcCodes();
const actual = ipc.map((entry) => entry?.code);
if (expected.length !== IPC_EXPECTED) {
  fail(`${SOURCE} 的 IpcErrorCode 变体数 ${expected.length} ≠ ${IPC_EXPECTED}`);
}
if (JSON.stringify(expected) !== JSON.stringify(actual)) {
  const mismatch = expected.findIndex((code, index) => code !== actual[index]);
  fail(
    `ipc 集合/顺序与 ${SOURCE} 不一致：首个差异在 index ${mismatch}（` +
      `期望 ${expected[mismatch] ?? "<EOF>"}，实际 ${actual[mismatch] ?? "<EOF>"}）`,
  );
}

let rendered;
try {
  rendered = renderErrorCodes(buildErrorCodes());
} catch (error) {
  fail(`重新生成失败：${error.message}`);
}
if (rendered !== undefined) {
  const current = readFileSync(path.join(root, TARGET), "utf8");
  if (current !== rendered) {
    fail(`${TARGET} 与重新生成结果不一致（生成物禁手改，diff 必须为 0）`);
  }
}

if (BUSINESS_SET_ENABLED) {
  const expectedBusiness = buildErrorCodes().business.map((entry) => entry.code);
  const actualBusiness = business.map((entry) => entry?.code);
  if (JSON.stringify(expectedBusiness) !== JSON.stringify(actualBusiness)) {
    fail(
      `business 集合与登记表不一致：期望 ${expectedBusiness.join(",")}；` +
        `实际 ${actualBusiness.join(",")}`,
    );
  }
} else {
  note("business 集合一致性校验自 M9-03 启用（本次只校验结构/去重/kind）。");
}

for (const message of notes) console.log(`[error-codes] 说明：${message}`);

if (failures.length > 0) {
  console.error(`[error-codes] 失败 ${failures.length} 项：`);
  for (const message of failures) console.error(`  - ${message}`);
  process.exit(1);
}

console.log(
  `[error-codes] 通过：ipc=${IPC_EXPECTED}、business=${business.length}、` +
    `schema/去重/kind 合规、生成物 diff=0`,
);
if (!BUSINESS_SET_ENABLED) {
  console.log("[error-codes] 业务码集合校验：M9-03 启用（--business-set）");
}
console.log("AETHER_ERROR_CODES PASS");
