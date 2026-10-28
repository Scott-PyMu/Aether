#!/usr/bin/env node
/**
 * `pnpm verify:m9-04`（P2 计划 M9-04；ADR-029 D-029-4）。
 *
 * 可执行校验口径：
 *  1. 命令合同 63/63：`COMMANDS`/`command-manifest.json` 与 M9-01 基线名称一致；
 *     63 = 59 daemon + 4 shell；UI 暴露 61/63（`startup_get`/`startup_migrate` 保留）；
 *     §4.6/§16.1 分组唯一且计数一致；debug 探针不计入；生成物 diff=0。
 *  2. 参数矩阵：63 条命令各有合法样本；非法样本覆盖未知字段/类型/枚举/长度/ULID/
 *     路径/越界；基线 `rejects`/`semantic_cases` 全部可回放。
 *  3. HEAD 基线对照：除已登记差异（`messages_page`/`audit_query` 预算/cursor 增量）
 *     外 0 差异；增量只允许新增，不允许修改既有期望值。
 *  4. 错误码：32 枚举 + 9 业务码逐项覆盖；业务码走 `data.business_code`。
 *  5. 代表性 golden：13 条 / 8 组（会话、消息、健康、备份、权限、设置、审计、供应商）。
 *  6. 预算/cursor 契约：64KiB–1.5MiB、至少 1 条、不透明游标、互斥、零推进安全。
 *  7. 壳本地路径与 RPC 路由矩阵的测试用例名冻结。
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
  buildCommandManifest,
  renderCommandManifest,
} from "../../ci/command-manifest.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checked = {
  commands: 0,
  baselineCommands: 0,
  matrixCommands: 0,
  matrixRejects: 0,
  goldenCases: 0,
  goldenGroups: 0,
};

function fail(message) {
  failures.push(message);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(root, rel));
}

function sha256File(rel) {
  return createHash("sha256").update(readFileSync(path.join(root, rel))).digest("hex");
}

function sourceWithoutComments(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
}

/** 只允许新增：旧值的每个键/数组元素必须原样保留（数组元素用 JSON 相等匹配）。 */
function assertNoRemoval(before, after, label, at = "") {
  if (Array.isArray(before)) {
    if (!Array.isArray(after)) {
      fail(`${label}${at} 必须是数组`);
      return;
    }
    for (const [index, item] of before.entries()) {
      const found = after.some((candidate) => JSON.stringify(candidate) === JSON.stringify(item));
      if (!found) {
        fail(`${label}${at}[${index}] 既有数组元素被修改/删除`);
      }
    }
    return;
  }
  if (before !== null && typeof before === "object") {
    if (after === null || typeof after !== "object" || Array.isArray(after)) {
      fail(`${label}${at} 必须是对象`);
      return;
    }
    for (const [key, value] of Object.entries(before)) {
      if (!(key in after)) {
        fail(`${label}${at}.${key} 被删除`);
        continue;
      }
      assertNoRemoval(value, after[key], label, `${at}.${key}`);
    }
    return;
  }
  if (before !== after) {
    fail(`${label}${at} 既有期望值被修改（${JSON.stringify(before)} → ${JSON.stringify(after)}）`);
  }
}

// ===== 1. 命令合同 63/63 + 生成物 diff=0 =====
const manifestPath = "packages/protocol/src/command-manifest.json";
if (!exists(manifestPath)) {
  fail(`[DoD-1] 缺少生成物 ${manifestPath}（先运行 pnpm generate:command-manifest）`);
} else {
  const rendered = renderCommandManifest(buildCommandManifest());
  if (read(manifestPath) !== rendered) {
    fail(`[DoD-1] ${manifestPath} 与重新生成结果不一致（生成物禁手改，diff 必须为 0）`);
  }
  const manifest = readJson(manifestPath);
  checked.commands = manifest.commands?.length ?? 0;
  if (manifest.counts?.total !== 63) fail(`[DoD-1] total=${manifest.counts?.total} ≠ 63`);
  if (manifest.counts?.daemon !== 59) fail(`[DoD-1] daemon=${manifest.counts?.daemon} ≠ 59`);
  if (manifest.counts?.shell !== 4) fail(`[DoD-1] shell=${manifest.counts?.shell} ≠ 4`);
  if (manifest.counts?.ui !== 61) fail(`[DoD-1] ui=${manifest.counts?.ui} ≠ 61`);
  if (manifest.counts?.reserved !== 2) fail(`[DoD-1] reserved=${manifest.counts?.reserved} ≠ 2`);
  if (manifest.counts?.groups !== 18) fail(`[DoD-1] groups=${manifest.counts?.groups} ≠ 18`);
  const names = (manifest.commands ?? []).map((entry) => entry?.name);
  if (new Set(names).size !== names.length) fail("[DoD-1] 命令名重复");
  for (const name of names) {
    if (/^(e2e_|debug_|test_)/.test(name)) fail(`[DoD-1] debug 探针不得计入 63：${name}`);
  }
  const reserved = (manifest.commands ?? []).filter((entry) => entry.exposure === "reserved");
  if (reserved.length !== 2) fail(`[DoD-1] reserved 暴露 ${reserved.length} ≠ 2`);
  for (const name of ["startup_get", "startup_migrate"]) {
    if (!reserved.some((entry) => entry.name === name)) {
      fail(`[DoD-1] ${name} 必须标记 reserved（UI 不暴露）`);
    }
  }
  const shell = (manifest.commands ?? []).filter((entry) => entry.route === "shell");
  for (const name of ["app_exit", "app_restart", "ref_pick", "startup_pick_target"]) {
    if (!shell.some((entry) => entry.name === name)) fail(`[DoD-1] 壳本地命令缺少 ${name}`);
  }
  const groupCounts = new Map((manifest.groups ?? []).map((group) => [group.group, group.count]));
  for (const entry of manifest.commands ?? []) {
    const expected = groupCounts.get(entry.group);
    if (!expected) fail(`[DoD-1] ${entry.name} 的分组 ${entry.group} 未登记`);
    if (!["daemon", "shell"].includes(entry.route)) fail(`[DoD-1] ${entry.name} 路由非法`);
  }
}

const baselineManifest = readJson("docs/M9-01-baseline/manifest.json");
const baselineNames = baselineManifest.commands ?? [];
if (baselineNames.length !== 63) fail(`[DoD-1] 基线命令 ${baselineNames.length} ≠ 63`);
const manifestNames = (readJson(manifestPath).commands ?? []).map((entry) => entry.name);
if (JSON.stringify(baselineNames) !== JSON.stringify(manifestNames)) {
  fail("[DoD-1] command-manifest 顺序/名称与 M9-01 基线不一致");
}
// 迁移期 commands.rs 的 `collect_commands!` 必须同序同名（M9-02 薄适配口径）。
const tauriCommands = read("crates/aether-tauri/src/ipc/commands.rs");
const collect = tauriCommands.match(/tauri_specta::collect_commands!\[([\s\S]*?)\n {4}\]/);
if (!collect) {
  fail("[DoD-1] 无法解析 tauri_specta::collect_commands!");
} else {
  const declared = collect[1]
    .split(/\r?\n/)
    .map((line) => line.trim().replace(/,$/, ""))
    .filter(Boolean);
  if (JSON.stringify(declared) !== JSON.stringify(manifestNames)) {
    fail("[DoD-1] collect_commands! 与 command-manifest 顺序/名称不一致");
  }
}

// ===== 2. 参数矩阵（63 条合法 + 非法类别）=====
const matrix = readJson("scripts/test/m9-04/fixtures/command-matrix.json");
if (matrix.schema !== "aether.m9-04.command-matrix.v1") {
  fail(`[DoD-2] 矩阵 schema 非法：${matrix.schema}`);
}
const matrixCommands = matrix.commands ?? [];
checked.matrixCommands = matrixCommands.length;
if (matrixCommands.length !== 63) fail(`[DoD-2] 合法矩阵 ${matrixCommands.length} ≠ 63`);
const validNames = matrixCommands.map((entry) => entry.command);
if (new Set(validNames).size !== 63) fail("[DoD-2] 合法矩阵命令重复");
for (const name of baselineNames) {
  if (!validNames.includes(name)) fail(`[DoD-2] 合法矩阵缺少命令 ${name}`);
}
const categories = new Set();
for (const entry of matrixCommands) {
  if (!("valid" in entry)) fail(`[DoD-2] ${entry.command} 缺少合法样本`);
  for (const item of entry.extra_invalid ?? []) {
    categories.add(item.code);
  }
}
const extraExpected = [
  "invalid_enum",
  "too_large",
  "out_of_range",
  "invalid_format",
  "invalid_value",
  "path_rejected",
  "artifact_path_rejected",
];
for (const code of extraExpected) {
  if (!categories.has(code)) fail(`[DoD-2] 非法矩阵缺少类别 ${code}`);
}

const baselineDir = path.join(root, "docs/M9-01-baseline/commands");
let baselineFiles = 0;
const baselineCodes = new Set();
for (const name of readdirSync(baselineDir).sort()) {
  if (!name.endsWith(".json")) continue;
  baselineFiles += 1;
  const entry = JSON.parse(readFileSync(path.join(baselineDir, name), "utf8"));
  checked.baselineCommands += 1;
  for (const item of [...(entry.rejects ?? []), ...(entry.semantic_cases ?? [])]) {
    baselineCodes.add(item.expect?.code);
  }
  checked.matrixRejects += (entry.rejects ?? []).length;
}
if (baselineFiles !== 63) fail(`[DoD-2] 基线命令文件 ${baselineFiles} ≠ 63`);
for (const code of ["unknown_field", "invalid_json", "missing_field", "invalid_type", "out_of_range"]) {
  if (!baselineCodes.has(code)) fail(`[DoD-2] 基线 rejects 缺少 ${code}`);
}

// ===== 3. HEAD 基线对照（只允许已登记差异）=====
const before = readJson("scripts/test/m9-04/fixtures/baseline-before.json");
const ALLOWED_COMMAND_DIFFS = new Set([
  "docs/M9-01-baseline/commands/12-messages_page.json",
  "docs/M9-01-baseline/commands/57-audit_query.json",
]);
const ALLOWED_ARTIFACT_DIFFS = new Set(["docs/M9-01-baseline/contract/responses.json"]);
const currentEntries = new Map((baselineManifest.entries ?? []).map((entry) => [entry.path, entry.sha256]));
const beforeEntries = new Map((before.entries ?? []).map((entry) => [entry.path, entry.sha256]));
if (beforeEntries.size !== 63) fail(`[DoD-3] before 快照条目 ${beforeEntries.size} ≠ 63`);
if (currentEntries.size !== 63) fail(`[DoD-3] 当前基线条目 ${currentEntries.size} ≠ 63`);
for (const [rel, hash] of beforeEntries) {
  if (!currentEntries.has(rel)) {
    fail(`[DoD-3] 基线条目被删除：${rel}`);
    continue;
  }
  if (currentEntries.get(rel) !== hash && !ALLOWED_COMMAND_DIFFS.has(rel)) {
    fail(`[DoD-3] 未登记差异：${rel}（M9-04 只允许 messages_page/audit_query 预算字段）`);
  }
}
for (const rel of ALLOWED_COMMAND_DIFFS) {
  if (!currentEntries.has(rel)) fail(`[DoD-3] 已登记差异文件缺失：${rel}`);
}
const currentArtifacts = new Map((baselineManifest.artifacts ?? []).map((item) => [item.path, item.sha256]));
for (const item of before.artifacts ?? []) {
  if (!currentArtifacts.has(item.path)) {
    fail(`[DoD-3] 辅助产物被删除：${item.path}`);
    continue;
  }
  if (currentArtifacts.get(item.path) !== item.sha256 && !ALLOWED_ARTIFACT_DIFFS.has(item.path)) {
    fail(`[DoD-3] 未登记差异：${item.path}`);
  }
}

// 已登记差异必须只新增、不修改既有期望值。
const beforeMessages = readJson("scripts/test/m9-04/fixtures/baseline-before/messages_page.json");
const beforeAudit = readJson("scripts/test/m9-04/fixtures/baseline-before/audit_query.json");
assertNoRemoval(beforeMessages, readJson("docs/M9-01-baseline/commands/12-messages_page.json"), "messages_page");
assertNoRemoval(beforeAudit, readJson("docs/M9-01-baseline/commands/57-audit_query.json"), "audit_query");
const responses = readJson("docs/M9-01-baseline/contract/responses.json");
const messagesResponse = (responses.types ?? []).find((type) => type.name === "MessagesPageResponse");
if (!messagesResponse) {
  fail("[DoD-3] responses.json 缺少 MessagesPageResponse");
} else {
  const beforeFields = readJson(
    "scripts/test/m9-04/fixtures/baseline-before/messages-page-response-fields.json",
  );
  assertNoRemoval(beforeFields.fields, messagesResponse.fields, "MessagesPageResponse.fields");
  for (const field of ["next_cursor", "messages_has_more"]) {
    if (!messagesResponse.fields.some((item) => item.name === field)) {
      fail(`[DoD-3] 已登记差异缺少响应字段 ${field}`);
    }
  }
}

// ===== 4. 32 枚举 + 9 业务码 =====
const errorMatrix = readJson("scripts/test/m9-04/fixtures/error-code-matrix.json");
const errorCodes = readJson("packages/protocol/src/error_codes.json");
const ipcCodes = errorMatrix.ipc ?? [];
const businessCodes = errorMatrix.business ?? [];
if (ipcCodes.length !== 32 || new Set(ipcCodes.map((entry) => entry.code)).size !== 32) {
  fail(`[DoD-2] 错误码矩阵 ipc ${ipcCodes.length} ≠ 32（或重复）`);
}
if (businessCodes.length !== 9 || new Set(businessCodes.map((entry) => entry.code)).size !== 9) {
  fail(`[DoD-2] 错误码矩阵 business ${businessCodes.length} ≠ 9（或重复）`);
}
for (const entry of ipcCodes) {
  if (!errorCodes.ipc.some((code) => code.code === entry.code)) {
    fail(`[DoD-2] 错误码矩阵 ipc ${entry.code} 不在 error_codes.json`);
  }
}
for (const entry of businessCodes) {
  if (!errorCodes.business.some((code) => code.code === entry.code)) {
    fail(`[DoD-2] 错误码矩阵 business ${entry.code} 不在 error_codes.json`);
  }
}
if (errorCodes.ipc.length !== 32) fail(`[DoD-2] error_codes.json ipc ${errorCodes.ipc.length} ≠ 32`);
if (errorCodes.business.length !== 9) fail(`[DoD-2] error_codes.json business ${errorCodes.business.length} ≠ 9`);

// ===== 5. 代表性 golden =====
const golden = readJson("scripts/test/m9-04/fixtures/golden.json");
if (golden.schema !== "aether.m9-04.golden.v1") fail(`[DoD-3] golden schema 非法：${golden.schema}`);
const goldenCases = golden.cases ?? [];
checked.goldenCases = goldenCases.length;
const goldenGroups = new Set(goldenCases.map((entry) => entry.group));
checked.goldenGroups = goldenGroups.size;
if (goldenCases.length < 13) fail(`[DoD-3] golden 用例 ${goldenCases.length} < 13`);
if (goldenGroups.size < 8) fail(`[DoD-3] golden 分组 ${goldenGroups.size} < 8`);
for (const required of [
  "session_create",
  "session_send",
  "messages_page",
  "health",
  "backup_create",
  "backup_list",
  "backup_restore",
  "permissions_pending",
  "permission_resolve",
  "settings_get",
  "settings_set",
  "audit_query",
  "providers_list",
]) {
  if (!goldenCases.some((entry) => entry.command === required)) {
    fail(`[DoD-3] golden 缺少 ${required}`);
  }
}

// ===== 6. 预算/cursor 契约（源码冻结）=====
const cursorSource = sourceWithoutComments(read("crates/aether-app/src/ipc/cursor.rs"));
for (const needle of [
  "pub const MAX_BYTES_MIN: u32 = 64 * 1024;",
  "pub const MAX_BYTES_DEFAULT: u32 = 1024 * 1024;",
  "pub const MAX_BYTES_MAX: u32 = 1536 * 1024;",
  'pub const STREAM_EVENTS: &str = "events";',
  'pub const STREAM_AUDIT: &str = "audit";',
  "pub fn fit_budget",
  "pub fn fit_budget_tail",
  "pub fn encode_cursor",
  "pub fn decode_cursor",
]) {
  if (!cursorSource.includes(needle)) fail(`[DoD-4] cursor 契约缺少：${needle}`);
}
const dtoSource = read("crates/aether-app/src/ipc/dto.rs");
for (const field of ["pub max_bytes: Option<u32>", "pub next_cursor: Option<String>"]) {
  const count = dtoSource.split(field).length - 1;
  if (count < 2) fail(`[DoD-4] messages_page/audit_query 必须都声明 ${field}（实际 ${count}）`);
}
for (const needle of [
  "next_cursor 与 last_seq 互斥",
  "next_cursor 与 offset 互斥",
  "filter_fingerprint",
]) {
  if (!dtoSource.includes(needle)) fail(`[DoD-4] DTO 缺少 ${needle}`);
}
const sessionSource = sourceWithoutComments(read("crates/aether-app/src/session_backend.rs"));
for (const needle of ["next_cursor", "messages_has_more", "value_bytes", "FRAME_LIMIT_BYTES"]) {
  if (!sessionSource.includes(needle)) fail(`[DoD-4] messages_page 缺少 ${needle}`);
}
const archiveSource = sourceWithoutComments(read("crates/aether-app/src/archive_control.rs"));
for (const needle of ["next_cursor", "cursor::fit_budget", "has_more"]) {
  if (!archiveSource.includes(needle)) fail(`[DoD-4] audit_query 缺少 ${needle}`);
}
const errorSource = sourceWithoutComments(read("crates/aether-app/src/ipc/error.rs"));
if (!errorSource.includes("business: Option<BusinessCode>")) {
  fail("[DoD-2] IpcError 缺少业务码承载字段");
}
if (!/#\[serde\(skip\)\]\s*\n\s*business: Option<BusinessCode>/.test(errorSource)) {
  fail("[DoD-2] IpcError.business 必须 serde(skip)（不改变 M9-01 错误形状）");
}
const serveSource = read("crates/aether-daemon/src/serve.rs");
if (!serveSource.includes("failure = failure.with_business(business.as_str())")) {
  fail("[DoD-2] map_ipc_error 必须写入 data.business_code");
}

// ===== 7. 测试用例名冻结 =====
const requiredTests = {
  "crates/aether-app/tests/m9_04_contract.rs": [
    "m9_04_baseline_reject_matrix_covers_all_63_commands",
    "m9_04_valid_request_matrix_covers_all_63_commands",
    "m9_04_error_code_matrix_is_32_plus_9_and_all_reachable",
    "m9_04_budget_and_cursor_contract_is_exposed_via_commands",
    "m9_04_shell_local_exit_restart_pick_and_cancel_paths",
    "m9_04_messages_next_cursor_is_opaque_and_zero_progress_safe",
  ],
  "crates/aether-app/tests/m9_04_golden.rs": [
    "m9_04_representative_goldens_match_structured_responses",
    "m9_04_payload_wrapper_is_rejected_for_golden_requests",
  ],
  "crates/aether-daemon/tests/m9_04_routes.rs": [
    "all_63_commands_are_routed_at_the_rpc_layer",
    "budget_and_cursor_failures_do_not_restart_the_daemon",
  ],
  "crates/aether-daemon/src/serve.rs": [
    "all_32_ipc_codes_map_to_transport_and_aether_code",
    "all_9_business_codes_are_carried_in_error_data",
  ],
  "crates/aether-tauri/tests/m3_02_session_backend.rs": [
    "messages_page_budget_truncates_cursor_resumes_and_zero_progress_is_safe",
  ],
  "crates/aether-tauri/tests/m6_03_audit.rs": [
    "audit_query_budget_truncates_cursor_resumes_and_tamper_is_rejected",
  ],
};
for (const [rel, names] of Object.entries(requiredTests)) {
  if (!exists(rel)) {
    fail(`[DoD-2/5] 缺少测试文件 ${rel}`);
    continue;
  }
  const source = read(rel);
  for (const name of names) {
    if (!source.includes(`fn ${name}(`)) fail(`[DoD-2/5] ${rel} 缺少用例 ${name}`);
  }
}

// ===== 8. 脚本与证据 =====
const packageJson = readJson("package.json");
if (!packageJson.scripts?.["verify:m9-04"]) {
  fail("[DoD-6] package.json 缺少 verify:m9-04");
}
if (!packageJson.scripts?.["generate:command-manifest"]) {
  fail("[DoD-1] package.json 缺少 generate:command-manifest");
}
const protocolPackage = readJson("packages/protocol/package.json");
for (const script of ["generate", "check"]) {
  if (!protocolPackage.scripts?.[script]?.includes("command-manifest.mjs")) {
    fail(`[DoD-1] @aether/protocol 的 ${script} 未接入 command-manifest`);
  }
}
const evidence = "docs/M9-04-证据.md";
if (!exists(evidence)) {
  fail(`[DoD-6] 缺少证据文档 ${evidence}`);
} else {
  const text = read(evidence);
  for (const section of ["DoD-1", "DoD-2", "DoD-3", "DoD-4", "DoD-5", "DoD-6"]) {
    if (!text.includes(`| ${section} `)) fail(`[证据] ${evidence} 缺少 ${section} 对照行`);
  }
}

// ===== 汇总 =====
if (failures.length > 0) {
  console.error(`[m9-04] FAIL（${failures.length} 项）`);
  for (const message of failures) console.error(`  - ${message}`);
  process.exit(1);
}

console.log(
  `[m9-04] OK：命令 ${checked.commands}（59 daemon + 4 shell；UI 61 / 保留 2）· ` +
    `基线条目 ${checked.baselineCommands}（已登记差异 2 命令 + 1 响应类型）· ` +
    `矩阵 ${checked.matrixCommands} 条 / rejects ${checked.matrixRejects} · ` +
    `错误码 32+9 · golden ${checked.goldenCases}/${checked.goldenGroups} 组 · ` +
    `预算 64KiB–1.5MiB / 不透明游标 · RPC 路由与壳本地用例齐备`,
);
console.log("AETHER_M9_04 PASS");
