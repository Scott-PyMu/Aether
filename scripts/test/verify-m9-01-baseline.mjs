#!/usr/bin/env node
/**
 * `pnpm verify:m9-01-baseline`（P2 计划 M9-01 DoD-9；ADR-029 §9.4-1）。
 *
 * 校验口径（基线存在性/哈希/条目数=63，外加冻结面的可执行护栏）：
 *  1. `docs/M9-01-baseline/manifest.json` 存在且 schema/版本/必填字段齐备；
 *  2. `commands[63]` 与当前 `collect_commands!` 声明**逐项一致**（顺序 + 名称）；
 *  3. `entries` 恰 63 条、路径唯一、文件存在、sha256 匹配；命令与条目一一对应；
 *  4. 计数自洽：63 = 59 daemon + 4 shell；61 可达 + 2 保留；ipc=32、business=9、
 *     事件信封 9 字段、设置 9 键、迁移 9 个；
 *  5. 辅助产物（contract/freeze）存在且哈希匹配，`contract_digest` 与当前源码重算一致
 *     （源码契约漂移即失败，防止「改了实现却没重录基线」）；
 *  6. 冻结面护栏：Electron 精确 patch（任何 package.json 的 electron 声明必须精确等于
 *     冻结版本）；硬阈值与冻结值一致（安装包 ≤1.5× HEAD 且 ≤450 MiB、idle RSS ≤500 MiB、
 *     10 会话 ≤1.2 GiB、启动/RSS/事件延迟阈值）；
 *  7. 每条命令基线的可证明拒绝用例齐备（unknown_field / invalid_json，含必填字段者
 *     另有 missing_field / invalid_type），分页边界仅 messages_page/audit_query 出现。
 *  8. 新增冻结面：`contract/responses.json`（命令响应恒为自由 JSON、类型 id 唯一、字段可选性齐备）、
 *     `contract/event_payloads.json`（23 类事件 / MVP 19 / 字段可选性）、`contract/stats.json`
 *     （DoD-3 关键值）、`contract/risks.json`（DoD-5 八项，含 owner/缓解/触发）、
 *     `contract/decisions.json`（DoD-6/DoD-7 两项裁定）。
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
  BASELINE_DIR,
  EXPECTED_COMMANDS,
  computeContractDigest,
} from "../ci/m9-01-baseline.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const failures = [];
const checked = { entries: 0, artifacts: 0, packages: 0 };

const SHELL_COMMANDS = ["app_exit", "app_restart", "ref_pick", "startup_pick_target"];
const RESERVED_COMMANDS = ["startup_get", "startup_migrate"];

const EXPECTED_THRESHOLDS = {
  installer_bytes: 127811584,
  installer_max_bytes: 191717376,
  absolute_cap_bytes: 471859200,
  idle_rss_max_bytes: 524288000,
  ten_sessions_max_bytes: 1258291200,
  window_shown_max_ms: 1500,
  handshake_target_ms: 2000,
  handshake_timeout_ms: 10000,
  ui_ready_max_ms: 5000,
  session_create_max_ms: 2000,
  p95_latency_max_ms: 150,
};

function fail(message) {
  failures.push(message);
}

function sha256File(rel) {
  return createHash("sha256").update(readFileSync(path.join(root, rel))).digest("hex");
}

function readJson(rel) {
  return JSON.parse(readFileSync(path.join(root, rel), "utf8"));
}

function readJsonOrNull(rel) {
  try {
    return readJson(rel);
  } catch {
    return null;
  }
}

function currentCommandNames() {
  const source = readFileSync(
    path.join(root, "crates/aether-tauri/src/ipc/commands.rs"),
    "utf8",
  );
  const block = source.match(/tauri_specta::collect_commands!\[([\s\S]*?)\n {4}\]/);
  if (!block) throw new Error("无法解析 collect_commands!（命令面契约）");
  return block[1]
    .split(/\r?\n/)
    .map((line) => line.trim().replace(/,$/, ""))
    .filter(Boolean);
}

const manifestPath = `${BASELINE_DIR}/manifest.json`;
if (!existsSync(path.join(root, manifestPath))) {
  console.error(`[m9-01-baseline] 阻断：基线缺失（${manifestPath}）。`);
  console.error("  先运行 `node scripts/ci/m9-01-baseline.mjs record`。");
  process.exit(1);
}

let manifest;
try {
  manifest = readJson(manifestPath);
} catch (error) {
  console.error(`[m9-01-baseline] 阻断：${manifestPath} 无法解析：${error.message}`);
  process.exit(1);
}

for (const key of [
  "schema",
  "version",
  "recorded_at",
  "head_commit",
  "platform",
  "counts",
  "commands",
  "entries",
  "artifacts",
  "contract_digest",
]) {
  if (!(key in manifest)) fail(`manifest 缺少字段 ${key}`);
}

if (manifest.schema !== "aether.m9-01-baseline.v1") {
  fail(`manifest.schema 必须是 aether.m9-01-baseline.v1，实际 ${manifest.schema}`);
}
if (manifest.version !== 1) fail(`manifest.version 必须是 1，实际 ${manifest.version}`);
if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}$/.test(String(manifest.recorded_at))) {
  fail(`manifest.recorded_at 必须是带时区的 ISO 时间，实际 ${manifest.recorded_at}`);
}
if (!/^[0-9a-f]{40}$/.test(String(manifest.head_commit))) {
  fail(`manifest.head_commit 必须是 40 位 commit id，实际 ${manifest.head_commit}`);
}

const commands = Array.isArray(manifest.commands) ? manifest.commands : [];
if (commands.length !== EXPECTED_COMMANDS) {
  fail(`manifest.commands 必须是 ${EXPECTED_COMMANDS} 项，实际 ${commands.length}`);
}
if (new Set(commands).size !== commands.length) fail("manifest.commands 存在重复命令");

const fromSource = currentCommandNames();
if (JSON.stringify(commands) !== JSON.stringify(fromSource)) {
  const at = fromSource.findIndex((name, index) => name !== commands[index]);
  fail(
    `manifest.commands 与 collect_commands! 不一致（index ${at}：` +
      `期望 ${fromSource[at] ?? "<EOF>"}，实际 ${commands[at] ?? "<EOF>"}）。` +
      "命令面变更必须先走 ADR，再重录基线。",
  );
}

const entries = Array.isArray(manifest.entries) ? manifest.entries : [];
if (entries.length !== EXPECTED_COMMANDS) {
  fail(`manifest.entries 必须恰 ${EXPECTED_COMMANDS} 条，实际 ${entries.length}`);
}
const entryCommands = new Map();
const seenPaths = new Set();
for (const entry of entries) {
  if (!entry || typeof entry.path !== "string" || typeof entry.sha256 !== "string") {
    fail("entries 元素必须含 path/sha256");
    continue;
  }
  if (seenPaths.has(entry.path)) fail(`entries 路径重复：${entry.path}`);
  seenPaths.add(entry.path);
  if (entry.path.includes("\\")) fail(`entries 路径必须用 / 分隔：${entry.path}`);
  if (!entry.path.startsWith(`${BASELINE_DIR}/commands/`)) {
    fail(`命令基线路径必须位于 ${BASELINE_DIR}/commands/：${entry.path}`);
  }
  if (!entry.command || !commands.includes(entry.command)) {
    fail(`条目 ${entry.path} 的 command 不在 manifest.commands 中：${entry.command}`);
  }
  if (entryCommands.has(entry.command)) {
    fail(`命令 ${entry.command} 对应多条基线条目`);
  }
  entryCommands.set(entry.command, entry.path);
  if (!existsSync(path.join(root, entry.path))) {
    fail(`基线文件缺失：${entry.path}`);
    continue;
  }
  const actual = sha256File(entry.path);
  if (actual !== entry.sha256) {
    fail(`基线文件哈希不匹配：${entry.path}（期望 ${entry.sha256}，实际 ${actual}）`);
  }
  checked.entries += 1;
}
for (const command of commands) {
  if (!entryCommands.has(command)) fail(`命令 ${command} 缺少基线条目`);
}

// ---- 计数自洽 ----
const counts = manifest.counts ?? {};
const shell = commands.filter((name) => SHELL_COMMANDS.includes(name)).length;
const daemon = commands.length - shell;
const reachable = commands.filter((name) => !RESERVED_COMMANDS.includes(name)).length;
const expectedCounts = {
  commands: EXPECTED_COMMANDS,
  reachable,
  reserved: RESERVED_COMMANDS.length,
  daemon,
  shell,
  ipc_error_codes: 32,
  business_codes: 9,
  event_envelope_fields: 9,
  settings_keys: 9,
  migrations: 9,
};
for (const [key, value] of Object.entries(expectedCounts)) {
  if (counts[key] !== value) fail(`counts.${key} 必须是 ${value}，实际 ${counts[key]}`);
}
if (daemon !== 59) fail(`daemon 路由必须是 59，实际 ${daemon}`);
if (shell !== 4) fail(`shell 路由必须是 4，实际 ${shell}`);

// ---- 辅助产物 ----
const artifacts = Array.isArray(manifest.artifacts) ? manifest.artifacts : [];
const EXPECTED_ARTIFACTS = [
  "contract/error_codes.json",
  "contract/events.json",
  "contract/settings.json",
  "contract/migrations.json",
  "contract/diagnostics.json",
  "contract/lifecycle.json",
  "contract/responses.json",
  "contract/event_payloads.json",
  "contract/stats.json",
  "contract/risks.json",
  "contract/decisions.json",
  "freeze/electron.json",
  "freeze/thresholds.json",
];
for (const suffix of EXPECTED_ARTIFACTS) {
  if (!artifacts.some((artifact) => artifact?.path === `${BASELINE_DIR}/${suffix}`)) {
    fail(`manifest.artifacts 缺少 ${suffix}`);
  }
}
for (const artifact of artifacts) {
  if (!artifact || typeof artifact.path !== "string" || typeof artifact.sha256 !== "string") {
    fail("artifacts 元素必须含 path/sha256");
    continue;
  }
  if (!existsSync(path.join(root, artifact.path))) {
    fail(`辅助产物缺失：${artifact.path}`);
    continue;
  }
  const actual = sha256File(artifact.path);
  if (actual !== artifact.sha256) {
    fail(`辅助产物哈希不匹配：${artifact.path}`);
  }
  checked.artifacts += 1;
}

// ---- 响应侧契约 / 事件 payload 形状 ----
const responses = readJsonOrNull(`${BASELINE_DIR}/contract/responses.json`);
if (!responses) {
  fail("contract/responses.json 缺失或不可解析");
} else {
  const responseCommands = Array.isArray(responses.commands) ? responses.commands : [];
  if (responseCommands.length !== EXPECTED_COMMANDS) {
    fail(`responses.commands 必须是 ${EXPECTED_COMMANDS} 项，实际 ${responseCommands.length}`);
  }
  for (const entry of responseCommands) {
    if (entry?.response?.kind !== "json" || entry?.response?.type !== null) {
      fail(`responses.commands[${entry?.command}] 必须是 {kind:"json",type:null}（HEAD 无按命令响应 DTO）`);
    }
  }
  const responseTypes = Array.isArray(responses.types) ? responses.types : [];
  if (responseTypes.length === 0) fail("responses.types 不能为空");
  if (!responses.command_envelope || responses.command_envelope.typed !== false) {
    fail("responses.command_envelope.typed 必须是 false");
  }
  if (typeof responses.dto_note !== "string" || responses.dto_note.length === 0) {
    fail("responses.dto_note 缺失");
  }
  const ids = new Set();
  for (const type of responseTypes) {
    if (typeof type?.id !== "string" || !type.id.includes("::")) {
      fail(`responses.types 元素缺少限定 id（<源文件>::<类型名>）：${type?.id}`);
      continue;
    }
    if (ids.has(type.id)) fail(`responses.types id 重复：${type.id}`);
    ids.add(type.id);
    if (type.kind === "struct") {
      if (!Array.isArray(type.fields)) fail(`${type.id} 缺少 fields`);
      for (const field of type.fields ?? []) {
        if (typeof field.optional !== "boolean" && typeof field.required !== "boolean") {
          fail(`${type.id}.${field.name} 缺少可选性标记`);
        }
      }
    } else if (!Array.isArray(type.variants) || type.variants.length === 0) {
      fail(`${type.id} 枚举缺少 variants`);
    }
  }
  if (counts.response_types !== responseTypes.length) {
    fail(`counts.response_types (${counts.response_types}) 与 responses.types (${responseTypes.length}) 不一致`);
  }
}

const eventPayloads = readJsonOrNull(`${BASELINE_DIR}/contract/event_payloads.json`);
if (!eventPayloads) {
  fail("contract/event_payloads.json 缺失或不可解析");
} else {
  const payloadTypes = Array.isArray(eventPayloads.types) ? eventPayloads.types : [];
  if (payloadTypes.length !== 23) {
    fail(`event_payloads.types 必须是 23（附录 B 全集），实际 ${payloadTypes.length}`);
  }
  const mvp = payloadTypes.filter((type) => type.mvp).length;
  if (mvp !== 19) fail(`event_payloads MVP 标记必须是 19 项，实际 ${mvp}`);
  if ((eventPayloads.envelope_fields ?? []).length !== 9) {
    fail("event_payloads.envelope_fields 必须是 9 项");
  }
  const wires = new Set();
  for (const type of payloadTypes) {
    if (typeof type?.type !== "string" || wires.has(type.type)) {
      fail(`event_payloads 类型非法或重复：${type?.type}`);
      continue;
    }
    wires.add(type.type);
    if (typeof type.payload !== "string" || !Array.isArray(type.fields)) {
      fail(`${type.type} 缺少 payload 类型或字段`);
    }
    for (const field of type.fields ?? []) {
      if (typeof field.optional !== "boolean") fail(`${type.type}.${field.name} 缺少可选性标记`);
    }
  }
  if (counts.event_payload_types !== payloadTypes.length) {
    fail(
      `counts.event_payload_types (${counts.event_payload_types}) 与 event_payloads.types (${payloadTypes.length}) 不一致`,
    );
  }
}

// ---- DoD-3 统计归档 ----
const stats = readJsonOrNull(`${BASELINE_DIR}/contract/stats.json`);
if (!stats) {
  fail("contract/stats.json 缺失或不可解析");
} else {
  const rules = [
    ["commands.total", stats.commands?.total, EXPECTED_COMMANDS],
    ["commands.daemon", stats.commands?.daemon, 59],
    ["commands.shell", stats.commands?.shell, 4],
    ["commands.reachable", stats.commands?.reachable, 61],
    ["protocol.dod_reference_bindings_lines", stats.protocol?.dod_reference_bindings_lines, 1003],
  ];
  for (const [path, actualValue, expected] of rules) {
    if (actualValue !== expected) fail(`stats.${path} 必须是 ${expected}，实际 ${actualValue}`);
  }
  for (const path of [
    "frontend.invoke_call_sites",
    "frontend.test_files",
    "protocol.bindings_lines",
    "core.lines",
    "core.rust_test_files",
  ]) {
    const value = path.split(".").reduce((node, key) => node?.[key], stats);
    if (!Number.isInteger(value) || value <= 0) fail(`stats.${path} 必须是正整数，实际 ${value}`);
  }
  if (stats.protocol?.matches_dod_reference !== (stats.protocol?.bindings_lines === 1003)) {
    fail("stats.protocol.matches_dod_reference 与 bindings_lines 不一致");
  }
}

// ---- DoD-5 风险登记 ----
const risks = readJsonOrNull(`${BASELINE_DIR}/contract/risks.json`);
if (!risks) {
  fail("contract/risks.json 缺失或不可解析");
} else {
  const list = Array.isArray(risks.risks) ? risks.risks : [];
  const requiredIds = [
    "electron_security_surface",
    "orphan_processes",
    "event_order",
    "keychain",
    "size_startup",
    "webcontentsview",
    "contract_drift",
    "dual_maintenance",
  ];
  if (list.length !== requiredIds.length) {
    fail(`risks 必须登记 ${requiredIds.length} 项，实际 ${list.length}`);
  }
  for (const id of requiredIds) {
    if (!list.some((risk) => risk?.id === id)) fail(`risks 缺少 ${id}`);
  }
  for (const risk of list) {
    for (const key of ["owner", "mitigation", "trigger"]) {
      if (typeof risk?.[key] !== "string" || risk[key].trim().length === 0) {
        fail(`risks.${risk?.id}.${key} 不能为空`);
      }
    }
  }
}

// ---- DoD-6/DoD-7 裁定 ----
const decisions = readJsonOrNull(`${BASELINE_DIR}/contract/decisions.json`);
if (!decisions) {
  fail("contract/decisions.json 缺失或不可解析");
} else {
  const list = Array.isArray(decisions.decisions) ? decisions.decisions : [];
  for (const id of ["linux_scope", "phase_boundary"]) {
    const decision = list.find((item) => item?.id === id);
    if (!decision) {
      fail(`decisions 缺少 ${id}`);
      continue;
    }
    for (const key of ["decision", "rationale", "owner"]) {
      if (typeof decision[key] !== "string" || decision[key].trim().length === 0) {
        fail(`decisions.${id}.${key} 不能为空`);
      }
    }
    for (const key of ["scope_in", "scope_out"]) {
      if (!Array.isArray(decision[key]) || decision[key].length === 0) {
        fail(`decisions.${id}.${key} 必须是非空数组`);
      }
    }
  }
}

// ---- 契约摘要（源码漂移即失败）----
let digest;
try {
  digest = computeContractDigest();
} catch (error) {
  fail(`重算 contract_digest 失败：${error.message}`);
}
if (digest && digest !== manifest.contract_digest) {
  fail(
    `contract_digest 与当前源码不一致（期望 ${manifest.contract_digest}，实际 ${digest}）。` +
      "契约变更必须先走 ADR 再重录基线。",
  );
}

// ---- 命令基线条目内容 ----
for (const command of commands) {
  const rel = entryCommands.get(command);
  if (!rel) continue;
  let entry;
  try {
    entry = readJson(rel);
  } catch (error) {
    fail(`${rel} 无法解析：${error.message}`);
    continue;
  }
  if (entry.command !== command) fail(`${rel} 的 command 字段与文件名不一致`);
  const expectedRoute = SHELL_COMMANDS.includes(command) ? "shell" : "daemon";
  if (entry.route !== expectedRoute) fail(`${rel} route 必须是 ${expectedRoute}`);
  const reserved = RESERVED_COMMANDS.includes(command);
  if (entry.reachable !== !reserved) fail(`${rel} reachable 必须是 ${!reserved}`);
  if (entry.exposure !== (reserved ? "reserved" : "ui")) {
    fail(`${rel} exposure 必须是 ${reserved ? "reserved" : "ui"}`);
  }
  const rejects = Array.isArray(entry.rejects) ? entry.rejects : [];
  if (entry.params?.style !== "none") {
    for (const id of ["unknown_field", "invalid_json"]) {
      if (!rejects.some((reject) => reject.id === id)) fail(`${rel} 缺少 rejects.${id}`);
    }
  }
  if (Array.isArray(entry.params?.required) && entry.params.required.length > 0) {
    for (const id of ["missing_field", "invalid_type"]) {
      if (!rejects.some((reject) => reject.id === id)) fail(`${rel} 缺少 rejects.${id}`);
    }
  }
  if (command === "messages_page" || command === "audit_query") {
    const semantic = Array.isArray(entry.semantic_cases) ? entry.semantic_cases : [];
    if (semantic.length === 0) fail(`${rel} 缺少分页/时间窗语义边界（semantic_cases）`);
  }
}

// ---- Electron 精确 patch ----
let electronFreeze;
try {
  electronFreeze = readJson(`${BASELINE_DIR}/freeze/electron.json`);
} catch (error) {
  fail(`freeze/electron.json 无法解析：${error.message}`);
}
if (electronFreeze) {
  if (!/^\d+\.\d+\.\d+$/.test(String(electronFreeze.version))) {
    fail(`electron.version 必须是精确 patch（x.y.z），实际 ${electronFreeze.version}`);
  }
  if (electronFreeze.pin !== "exact") fail("electron.pin 必须是 exact");
  if (String(electronFreeze.line) !== String(electronFreeze.version).split(".")[0]) {
    fail("electron.line 必须与 version 的主版本一致");
  }
  for (const rel of walkPackageJson(".")) {
    let pkg;
    try {
      pkg = readJson(rel);
    } catch {
      fail(`${rel} 无法解析`);
      continue;
    }
    for (const section of ["dependencies", "devDependencies", "optionalDependencies"]) {
      const declared = pkg?.[section]?.electron;
      if (declared === undefined) continue;
      checked.packages += 1;
      if (declared !== electronFreeze.version) {
        fail(
          `${rel} 的 ${section}.electron 必须精确锁定 ${electronFreeze.version}，实际 ${declared}`,
        );
      }
    }
  }
}

// ---- 硬阈值 ----
let thresholds;
try {
  thresholds = readJson(`${BASELINE_DIR}/freeze/thresholds.json`);
} catch (error) {
  fail(`freeze/thresholds.json 无法解析：${error.message}`);
}
if (thresholds) {
  const flat = {
    installer_bytes: thresholds.head_baseline?.installer_bytes,
    installer_max_bytes: thresholds.installer?.max_bytes,
    absolute_cap_bytes: thresholds.installer?.absolute_cap_bytes,
    idle_rss_max_bytes: thresholds.runtime?.idle_rss_max_bytes,
    ten_sessions_max_bytes: thresholds.runtime?.ten_sessions_max_bytes,
    window_shown_max_ms: thresholds.startup?.window_shown_max_ms,
    handshake_target_ms: thresholds.startup?.handshake_target_ms,
    handshake_timeout_ms: thresholds.startup?.handshake_timeout_ms,
    ui_ready_max_ms: thresholds.startup?.ui_ready_max_ms,
    session_create_max_ms: thresholds.startup?.session_create_max_ms,
    p95_latency_max_ms: thresholds.events?.p95_latency_max_ms,
  };
  for (const [key, expected] of Object.entries(EXPECTED_THRESHOLDS)) {
    if (flat[key] !== expected) fail(`thresholds.${key} 必须是 ${expected}，实际 ${flat[key]}`);
  }
  if (thresholds.installer?.max_bytes !== Math.floor(flat.installer_bytes * 1.5)) {
    fail("thresholds.installer.max_bytes 必须等于 floor(HEAD 基线 × 1.5)");
  }
  if (thresholds.installer?.max_bytes > thresholds.installer?.absolute_cap_bytes) {
    fail("thresholds.installer.max_bytes 不得超过绝对上限（450 MiB）");
  }
  if (thresholds.status !== "frozen") fail("thresholds.status 必须是 frozen");
  if (thresholds.events?.control_event_loss_allowed !== 0) {
    fail("thresholds.events.control_event_loss_allowed 必须是 0");
  }
}

function walkPackageJson(relDir, depth = 0) {
  if (depth > 4) return [];
  const absolute = path.join(root, relDir);
  const found = [];
  for (const dirent of readdirSync(absolute, { withFileTypes: true })) {
    const rel = path.posix.join(relDir.replace(/\\/g, "/"), dirent.name).replace(/^\.\//, "");
    if (dirent.isDirectory()) {
      if (["node_modules", "target", ".git", ".tmp", "dist-out", "coverage"].includes(dirent.name)) {
        continue;
      }
      found.push(...walkPackageJson(rel, depth + 1));
      continue;
    }
    if (dirent.name === "package.json") found.push(rel);
  }
  return found;
}

if (failures.length > 0) {
  console.error(`[m9-01-baseline] 失败 ${failures.length} 项：`);
  for (const message of failures) console.error(`  - ${message}`);
  process.exit(1);
}

console.log(
  `[m9-01-baseline] 通过：commands=${commands.length}（daemon ${daemon} / shell ${shell}，` +
    `可达 ${reachable} + 保留 ${RESERVED_COMMANDS.length}）、entries=${checked.entries}、` +
    `artifacts=${checked.artifacts}、响应类型=${responses?.types?.length ?? 0}、` +
    `事件 payload=${eventPayloads?.types?.length ?? 0}、risk=${risks?.risks?.length ?? 0}、` +
    `裁定=${decisions?.decisions?.length ?? 0}、electron=${electronFreeze?.version}（精确 patch）、` +
    `阈值全冻结、contract_digest 一致`,
);
console.log("AETHER_M901_BASELINE PASS");
