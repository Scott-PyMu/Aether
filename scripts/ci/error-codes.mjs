#!/usr/bin/env node
/**
 * `error_codes.json` 生成器（M9-01 DoD-4；ADR-029 §4.3/§7、AGENTS §2.8）。
 *
 * 单一事实来源（M9-01 骨架期；M9-03 起改由 `aether-app` 清单提供，输出格式不变）：
 * - `ipc`：`crates/aether-app/src/ipc/error.rs` 的 `IpcErrorCode` 枚举顺序 + `as_str()`
 *   映射（32 项）。代码名不得手写：解析不到、解析出多于/少于 32 项都直接失败。
 * - `business`：本文件 `BUSINESS_CODES` 登记表，逐项与 ADR-018 附录 A-7 的 9 个业务码
 *   一致（`aether_code` 只取 32 枚举；业务码走 `error.data.business_code`，ADR-029 R-P0-2）。
 *
 * `owner` = 首次引入该码的里程碑，`first_adr` = 首次登记它的 ADR：
 * - ipc 优先取自枚举变体上方文档注释里的第一个 `M<里程碑>-<序号>` / `ADR-<三位>`；
 * - 无里程碑注释的通用校验码按 M1-08（IPC 参数校验框架批次）登记；
 * - `first_adr` 未在注释中出现时输出 `null`，不臆造 ADR 编号。
 *
 * 用法：
 *   node scripts/ci/error-codes.mjs generate   # 写入 packages/protocol/src/error_codes.json
 *   node scripts/ci/error-codes.mjs check      # 重新生成并与工作区文件逐字节比较（不写文件）
 */
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const SOURCE = "crates/aether-app/src/ipc/error.rs";
const TARGET = "packages/protocol/src/error_codes.json";
const IPC_EXPECTED_COUNT = 32;
const FALLBACK_OWNER = "M1-08";

/**
 * business 集合（ADR-018 附录 A-7 的 9 项，顺序即登记顺序）。
 *
 * M9-01 只冻结「已登记集合」本身；集合与业务常量的一致性校验自 M9-03 启用
 * （`verify:error-codes --business-set`）。
 */
const BUSINESS_CODES = [
  { code: "session_busy", owner: "M5-01", first_adr: "ADR-018" },
  { code: "session_paused", owner: "M5-01", first_adr: "ADR-018" },
  { code: "storage_backpressure", owner: "M5-01", first_adr: "ADR-018" },
  { code: "memory_conflict", owner: "M5-01", first_adr: "ADR-018" },
  { code: "permission_not_pending", owner: "M5-01", first_adr: "ADR-018" },
  { code: "data_dir_locked", owner: "M9-03", first_adr: "ADR-029" },
  { code: "sync_dir_blocked", owner: "M5-01", first_adr: "ADR-018" },
  { code: "db_open_failed", owner: "M9-03", first_adr: "ADR-029" },
  { code: "schema_checksum_mismatch", owner: "M9-03", first_adr: "ADR-029" },
];

const MODE = process.argv[2] ?? "check";
if (MODE !== "generate" && MODE !== "check") {
  console.error(`[error-codes] 未知模式：${MODE}（可选 generate / check）`);
  process.exit(2);
}

/** 取源码中 `pub enum IpcErrorCode { ... }` 的枚举体。 */
function enumBody(source) {
  const match = source.match(/pub enum IpcErrorCode \{([\s\S]*?)\n\}/);
  if (!match) throw new Error(`${SOURCE}：未找到 IpcErrorCode 枚举`);
  return match[1];
}

/** 取 `as_str()` 的 `Self::X => "code"` 映射。 */
function asStrMap(source) {
  const match = source.match(
    /impl IpcErrorCode \{[\s\S]*?pub const fn as_str\(self\)[\s\S]*?\n {4}\}/,
  );
  if (!match) throw new Error(`${SOURCE}：未找到 IpcErrorCode::as_str`);
  const map = new Map();
  for (const arm of match[0].matchAll(/Self::(\w+)\s*=>\s*"([a-z0-9_]+)"/g)) {
    map.set(arm[1], arm[2]);
  }
  return map;
}

/** 解析枚举变体（声明顺序 + 变体上方的文档注释）。 */
function parseVariants(body) {
  const variants = [];
  let doc = [];
  for (const rawLine of body.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line.startsWith("///")) {
      doc.push(line.replace(/^\/\/\/\s?/, ""));
      continue;
    }
    if (line.startsWith("#[")) continue;
    const variant = line.match(/^([A-Z]\w+),$/);
    if (variant) {
      variants.push({ name: variant[1], doc: doc.join("\n") });
      doc = [];
      continue;
    }
    if (line !== "") doc = [];
  }
  return variants;
}

/** 从文档注释推导 owner / first_adr（不臆造：解析不到即 null / fallback）。 */
function provenance(doc) {
  const milestone = doc.match(/\bM\d{1,2}-\d{2}\b/);
  const adr = doc.match(/\bADR-\d{3}\b/);
  return {
    owner: milestone ? milestone[0] : FALLBACK_OWNER,
    first_adr: adr ? adr[0] : null,
  };
}

/** 构建 `{ ipc, business }`（生成物本体；`ipc` 顺序 = 枚举声明顺序）。 */
export function buildErrorCodes() {
  const source = readFileSync(path.join(root, SOURCE), "utf8");
  const variants = parseVariants(enumBody(source));
  const codes = asStrMap(source);
  if (variants.length !== IPC_EXPECTED_COUNT) {
    throw new Error(
      `[error-codes] IpcErrorCode 变体数 ${variants.length} ≠ ${IPC_EXPECTED_COUNT}：` +
        "枚举扩张必须走 ADR 并同步本生成器/基线（ADR-029 R-P0-2）。",
    );
  }
  const ipc = variants.map(({ name, doc }) => {
    const code = codes.get(name);
    if (!code) throw new Error(`[error-codes] 变体 ${name} 缺少 as_str() 映射`);
    return { code, kind: "ipc", ...provenance(doc) };
  });
  const business = BUSINESS_CODES.map((entry) => ({ ...entry, kind: "business" }));
  return { ipc, business };
}

/** 稳定序列化：2 空格缩进 + 末尾换行 + LF（跨平台逐字节可比）。 */
export function renderErrorCodes(model) {
  return `${JSON.stringify(model, null, 2)}\n`;
}

const rendered = renderErrorCodes(buildErrorCodes());
const targetPath = path.join(root, TARGET);

if (MODE === "generate") {
  writeFileSync(targetPath, rendered, "utf8");
  console.log(`[error-codes] 生成完成：${TARGET}（ipc=${IPC_EXPECTED_COUNT}）`);
  process.exit(0);
}

let current;
try {
  current = readFileSync(targetPath, "utf8");
} catch {
  console.error(`[error-codes] 阻断：生成物缺失（${TARGET}）。先运行 generate 并入库。`);
  process.exit(1);
}

if (current !== rendered) {
  const expected = rendered.split("\n");
  const actual = current.split("\n");
  const at = expected.findIndex((line, index) => line !== actual[index]);
  console.error(`[error-codes] 阻断：${TARGET} 与重新生成结果不一致（diff=0 要求）。`);
  console.error(`  首个差异：第 ${at + 1} 行`);
  console.error(`  期望：${expected[at] ?? "<EOF>"}`);
  console.error(`  实际：${actual[at] ?? "<EOF>"}`);
  process.exit(1);
}

console.log(`[error-codes] 通过：${TARGET} 与重新生成结果一致（ipc=${IPC_EXPECTED_COUNT}）`);
