#!/usr/bin/env node
/**
 * `pnpm verify:m9-03`（P2 计划 M9-03；ADR-029 v0.4 D-029-3/D-029-6）。
 *
 * 可执行校验口径（静态 + 契约；动态证据见 `docs/M9-03-证据.md`）：
 *  1. `aether-rpc` 为**叶子**（不依赖任何内部 crate），`aether-daemon` 只依赖
 *     `aether-app`/`aether-rpc`/核心库，**不依赖** `aether-tauri`/tauri/Electron；
 *  2. 帧上限 `MAX_FRAME_BYTES = 2 * 1024 * 1024`，读取器使用 `fill_buf` 有界消费
 *     （不 `read_line`/`read_to_end`/`read_until` 整行读入）；
 *  3. 传输码 12 项（含 `-32000…-32006`）与 ADR-029 §4.3 一致；
 *  4. `DaemonPhase` 8 个产出值 + `migrating`/`blocked` 保留值；
 *  5. `initialize` 结构（请求 5 字段 / 响应 6 字段 / capabilities 三值 + client-only）；
 *  6. `aetherd` 退出码 0/10/20/21/22/23/24；`--stdio`/`--version`/`--help`；
 *  7. params 形状 golden fixtures（有参/无参/空对象/`payload` 包装拒绝）齐备且可解析；
 *  8. 业务码常量（`BusinessCode` 9 项）与 `error_codes.json` 的 `business` 一致；
 *  9. 进程级测试覆盖 DoD-3/DoD-5/DoD-7 的关键断言（用例名冻结）。
 */
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const notes = [];
const checked = {
  rpcSources: 0,
  daemonSources: 0,
  fixtureCases: 0,
  fixtureLines: 0,
  testCases: 0,
};

function fail(message) {
  failures.push(message);
}

function note(message) {
  notes.push(message);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function stripComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
}

function rustSources(dir) {
  const result = [];
  const visit = (relative) => {
    const base = path.join(root, relative);
    for (const name of readdirSync(base)) {
      const nested = path.posix.join(relative, name);
      const full = path.join(root, nested);
      if (statSync(full).isDirectory()) visit(nested);
      else if (name.endsWith(".rs")) result.push(nested);
    }
  };
  visit(dir.replace(/\\/g, "/"));
  return result;
}

function dependenciesOf(manifestSource) {
  const block = manifestSource.match(/\[dependencies\]([\s\S]*?)(?:\n\[|$)/);
  if (!block) return new Map();
  const map = new Map();
  for (const line of block[1].split(/\r?\n/)) {
    const entry = line.match(/^([a-zA-Z0-9_-]+)(?:\.workspace)?\s*=/);
    if (entry) map.set(entry[1], line.trim());
  }
  return map;
}

// ===== DoD-1/DoD-6：crate 边界与叶子约束 =====
const workspace = read("Cargo.toml");
for (const member of ["crates/aether-rpc", "crates/aether-daemon"]) {
  if (!workspace.includes(`"${member}"`)) fail(`[DoD-1] workspace 未包含 ${member}`);
}
for (const dep of ["aether-rpc", "aether-daemon"]) {
  if (!new RegExp(`^${dep}\\s*=\\s*\\{`, "m").test(workspace)) {
    fail(`[DoD-1] workspace.dependencies 缺少 ${dep}`);
  }
}

const rpcManifest = stripComments(read("crates/aether-rpc/Cargo.toml"));
const rpcDeps = dependenciesOf(rpcManifest);
for (const name of rpcDeps.keys()) {
  if (name.startsWith("aether-")) {
    fail(`[DoD-1] aether-rpc 必须是叶子 crate，不得依赖内部 crate：${name}`);
  }
}
if (!/^name\s*=\s*"aether-rpc"/m.test(rpcManifest)) fail("[DoD-1] aether-rpc 包名不符");

const daemonManifest = stripComments(read("crates/aether-daemon/Cargo.toml"));
const daemonDeps = dependenciesOf(daemonManifest);
for (const required of ["aether-app", "aether-rpc"]) {
  if (!daemonDeps.has(required)) fail(`[DoD-1] aether-daemon 缺少依赖 ${required}`);
}
for (const forbidden of ["aether-tauri", "tauri", "wry", "webkit2gtk", "electron"]) {
  if (daemonDeps.has(forbidden)) {
    fail(`[DoD-1] aether-daemon 不得依赖 ${forbidden}（ADR-029 §3.3）`);
  }
}
if (!/^name\s*=\s*"aetherd"/m.test(daemonManifest)) fail("[DoD-6] bin 名必须是 aetherd");
if (!existsSync(path.join(root, "crates/aether-daemon/src/main.rs"))) {
  fail("[DoD-6] 缺少 crates/aether-daemon/src/main.rs");
}

for (const rel of rustSources("crates/aether-rpc/src")) {
  const source = stripComments(read(rel));
  checked.rpcSources += 1;
  if (/\buse tauri\b|\btauri::/.test(source)) fail(`[DoD-1] ${rel} 含 Tauri 依赖`);
  if (/\baether_(app|core|store|control|adapters|security)\b/.test(source)) {
    fail(`[DoD-1] ${rel} 依赖内部 crate（叶子约束）`);
  }
}
for (const rel of rustSources("crates/aether-daemon/src")) {
  const source = stripComments(read(rel));
  checked.daemonSources += 1;
  if (/\buse tauri\b|\btauri::/.test(source)) fail(`[DoD-1] ${rel} 含 Tauri 依赖`);
  if (/aether_tauri/.test(source)) fail(`[DoD-1] ${rel} 依赖 aether-tauri`);
}

// ===== DoD-1：2MiB 有界读取器 =====
const codecSource = read("crates/aether-rpc/src/codec.rs");
if (!/pub const MAX_FRAME_BYTES: usize = 2 \* 1024 \* 1024;/.test(codecSource)) {
  fail("[DoD-1] MAX_FRAME_BYTES 必须是 2 * 1024 * 1024");
}
if (!codecSource.includes("fill_buf")) fail("[DoD-1] 读取器必须使用 fill_buf 有界消费");
for (const forbidden of ["read_line(", "read_until(", "read_to_end(", "read_to_string("]) {
  if (codecSource.includes(forbidden)) {
    fail(`[DoD-1] codec 不得使用无界读取：${forbidden}`);
  }
}
if (!codecSource.includes("Frame::Oversized")) fail("[DoD-1] 缺少超限帧分支");
if (!/fn oversized_frame_is_rejected_without_buffering_and_next_frame_still_reads/.test(codecSource)) {
  fail("[DoD-1] 缺少「超限拒绝 + 重连续读」单测");
}

// ===== DoD-4：传输码命名空间 =====
const rpcLib = read("crates/aether-rpc/src/lib.rs");
const expectedCodes = [
  ["PARSE_ERROR", -32700],
  ["INVALID_REQUEST", -32600],
  ["METHOD_NOT_FOUND", -32601],
  ["INVALID_PARAMS", -32602],
  ["INTERNAL_ERROR", -32603],
  ["APPLICATION_ERROR", -32000],
  ["PROTOCOL_MISMATCH", -32001],
  ["CORE_NOT_READY", -32002],
  ["TOO_LARGE", -32003],
  ["SHUTTING_DOWN", -32004],
  ["BOOTSTRAP_FAILURE", -32005],
  ["CAPABILITY_MISSING", -32006],
];
for (const [name, value] of expectedCodes) {
  const pattern = new RegExp(`pub const ${name}: i32 = -?\\d+;`);
  const match = rpcLib.match(pattern);
  if (!match) {
    fail(`[DoD-4] 缺少传输码 ${name}`);
    continue;
  }
  if (!match[0].includes(String(value))) {
    fail(`[DoD-4] 传输码 ${name} 取值应为 ${value}：${match[0]}`);
  }
}

// `aether_code` 只取 32 枚举：aether-rpc/daemon 中的字面量必须在 ipc 集合内。
const ipcCodes = new Set(
  readJson("packages/protocol/src/error_codes.json").ipc.map((entry) => entry.code),
);
if (ipcCodes.size !== 32) fail(`[DoD-4] error_codes.json ipc 必须 32 项，实际 ${ipcCodes.size}`);
for (const rel of [
  ...rustSources("crates/aether-rpc/src"),
  ...rustSources("crates/aether-daemon/src"),
]) {
  const source = stripComments(read(rel));
  for (const match of source.matchAll(/aether_code:\s*"([a-z0-9_]+)"/g)) {
    if (!ipcCodes.has(match[1])) {
      fail(`[DoD-4] ${rel} 的 aether_code 字面量 ${match[1]} 不在 32 枚举内`);
    }
  }
  for (const match of source.matchAll(/"([a-z0-9_]+)"\s*,\s*\n?\s*\/\/\s*aether/gi)) {
    if (!ipcCodes.has(match[1])) {
      fail(`[DoD-4] ${rel} 可疑 aether_code 字面量：${match[1]}`);
    }
  }
}

// ===== DoD-2：params 形状冻结（无 payload 包装）=====
const dispatchSource = read("crates/aether-app/src/ipc/dispatch.rs");
if (!/pub const COMMANDS: &\[&str\] = &\[/.test(dispatchSource)) {
  fail("[DoD-2] 未找到 aether-app::COMMANDS 清单");
}
if (!/pub const SHELL_COMMANDS: &\[&str\] = &\[/.test(dispatchSource)) {
  fail("[DoD-2] 未找到壳本地命令清单（59/4 路由口径）");
}
if (dispatchSource.includes("payload`") && dispatchSource.includes("parse_payload")) {
  fail("[DoD-2] 命令层不得保留 `{payload}` 包装解析");
}

// ===== DoD-3：DaemonPhase / initialize =====
const initializeSource = read("crates/aether-rpc/src/initialize.rs");
const produced = initializeSource.match(/pub const PRODUCED: \[Self; (\d+)\] = \[([\s\S]*?)\n {4}\];/);
if (!produced) {
  fail("[DoD-3] 未找到 DaemonPhase::PRODUCED");
} else {
  if (Number(produced[1]) !== 8) fail(`[DoD-3] PRODUCED 必须 8 项，实际 ${produced[1]}`);
  const variants = [...produced[2].matchAll(/Self::(\w+)/g)].map((match) => match[1]);
  for (const reserved of ["Migrating", "Blocked"]) {
    if (variants.includes(reserved)) fail(`[DoD-3] ${reserved} 是保留值，不得出现在 PRODUCED`);
  }
}
for (const phase of [
  "starting",
  "detecting",
  "opening",
  "warming",
  "ready",
  "degraded",
  "shutting_down",
  "failed",
  "migrating",
  "blocked",
]) {
  if (!initializeSource.includes(`"${phase}"`)) fail(`[DoD-3] DaemonPhase 缺少 ${phase}`);
}
for (const field of [
  "protocol_version",
  "daemon_version",
  "core_version",
  "data_dir",
  "phase",
  "daemon_capabilities",
]) {
  if (!initializeSource.includes(`pub ${field}:`)) {
    fail(`[DoD-3] InitializeResult 缺少字段 ${field}`);
  }
}
for (const capability of ["events", "permissions", "dialogs"]) {
  if (!initializeSource.includes(`"${capability}"`)) {
    fail(`[DoD-3] daemonCapabilities 缺少 ${capability}`);
  }
}
if (!initializeSource.includes('"browser-view"')) {
  fail("[DoD-3] 缺少 client-only 能力 browser-view");
}
if (!/pub const PROTOCOL_VERSION: &str = "1\.0";/.test(initializeSource)) {
  fail('[DoD-3] PROTOCOL_VERSION 必须是 "1.0"');
}

// ===== DoD-3/DoD-7：shutdown 回执与关闭序列 =====
const connectionSource = read("crates/aether-rpc/src/connection.rs");
for (const needle of ["ReplyAndClose", "shutdown_id", "\"initialize\"", "\"shutdown\""]) {
  if (!connectionSource.includes(needle)) fail(`[DoD-3] 连接状态机缺少 ${needle}`);
}
if (!/fn duplicate_request_id_is_invalid_request/.test(connectionSource)) {
  fail("[DoD-7] 缺少重复 id 单测");
}

// ===== DoD-5：致命启动检查与退出码 =====
const bootstrapSource = read("crates/aether-daemon/src/bootstrap.rs");
const exitCodes = [
  ["EXIT_OK", 0],
  ["EXIT_USAGE", 10],
  ["EXIT_DATA_DIR_LOCKED", 20],
  ["EXIT_SYNC_DIR_BLOCKED", 21],
  ["EXIT_DB_OPEN_FAILED", 22],
  ["EXIT_MIGRATION_FAILED", 23],
  ["EXIT_INTERNAL", 24],
];
for (const [name, value] of exitCodes) {
  const match = bootstrapSource.match(new RegExp(`pub const ${name}: u8 = \\d+;`));
  if (!match) {
    fail(`[DoD-5] 缺少退出码常量 ${name}`);
    continue;
  }
  if (!match[0].includes(String(value))) fail(`[DoD-5] ${name} 应为 ${value}：${match[0]}`);
}
if (!bootstrapSource.includes("EXIT_MIGRATION_FAILED")) {
  fail("[DoD-5] 退出码 23 必须保留为码位（M9 不触发）");
}
for (const business of [
  "BusinessCode::DataDirLocked",
  "BusinessCode::SyncDirBlocked",
  "BusinessCode::DbOpenFailed",
  "BusinessCode::SchemaChecksumMismatch",
]) {
  if (!bootstrapSource.includes(business)) fail(`[DoD-5] 启动失败缺业务码 ${business}`);
}
if (!/aether_store::preflight/.test(bootstrapSource)) {
  fail("[DoD-5] 预检必须调用DB 打开 + 迁移 checksum 校验（aether_store::preflight）");
}
const storeLib = read("crates/aether-store/src/lib.rs");
if (!storeLib.includes("pub mod preflight;")) fail("[DoD-5] aether-store 缺少 preflight 模块");
const preflightSource = read("crates/aether-store/src/preflight.rs");
const preflightCode = stripComments(preflightSource).split("#[cfg(test)]")[0];
if (!preflightCode.includes("MigrationChecksumMismatch")) {
  fail("[DoD-5] 预检缺少 checksum 不符分支");
}
if (/quick_check\(/.test(preflightCode)) {
  fail("[DoD-5] 预检不得执行 quick_check（安全模式由预热阶段处理）");
}

// ===== DoD-5：业务码常量集合 =====
const businessSource = read("crates/aether-app/src/ipc/business.rs");
const businessMatch = businessSource.match(/pub const ALL: \[Self; (\d+)\] = \[([\s\S]*?)\n {4}\];/);
if (!businessMatch) {
  fail("[DoD-4] 未找到 BusinessCode::ALL");
} else {
  const mapping = new Map();
  const implBlock = businessSource.match(
    /pub const fn as_str\(self\) -> &'static str \{([\s\S]*?)\n {4}\}/,
  );
  for (const arm of implBlock?.[1].matchAll(/Self::(\w+)\s*=>\s*"([a-z0-9_]+)"/g) ?? []) {
    mapping.set(arm[1], arm[2]);
  }
  const rustCodes = [...businessMatch[2].matchAll(/Self::(\w+)/g)].map((match) =>
    mapping.get(match[1]),
  );
  const jsonCodes = readJson("packages/protocol/src/error_codes.json").business.map(
    (entry) => entry.code,
  );
  if (JSON.stringify(rustCodes) !== JSON.stringify(jsonCodes)) {
    fail(
      `[DoD-4] 业务码常量与 error_codes.json 不一致：Rust ${rustCodes.join(",")}；` +
        `JSON ${jsonCodes.join(",")}`,
    );
  }
  if (jsonCodes.length !== 9) fail(`[DoD-4] business 必须 9 项，实际 ${jsonCodes.length}`);
}
const packageJson = readJson("package.json");
if (!packageJson.scripts?.["verify:error-codes"]?.includes("--business-set")) {
  fail("[DoD-4] package.json 的 verify:error-codes 必须启用 --business-set");
}

// ===== DoD-2：golden fixtures =====
const fixtureDir = path.join(root, "scripts/test/m9-03/fixtures");
const fixtures = ["initialize.jsonl", "params.jsonl", "transport.jsonl", "shutdown.jsonl"];
for (const name of fixtures) {
  const full = path.join(fixtureDir, name);
  if (!existsSync(full)) {
    fail(`[DoD-2] 缺少 golden fixture ${name}`);
    continue;
  }
  const lines = readFileSync(full, "utf8")
    .split(/\r?\n/)
    .filter((line) => line.trim().length > 0);
  checked.fixtureLines += lines.length;
  lines.forEach((line, index) => {
    try {
      JSON.parse(line);
    } catch {
      // `transport.jsonl` 第 0 行是**故意的**畸形帧（parse_error golden）。
      if (name === "transport.jsonl" && index === 0) return;
      fail(`[DoD-2] ${name} 第 ${index} 行不是合法 JSON`);
    }
  });
}
const expectationsPath = path.join(fixtureDir, "expectations.json");
if (!existsSync(expectationsPath)) {
  fail("[DoD-2] 缺少 expectations.json");
} else {
  const expectations = JSON.parse(readFileSync(expectationsPath, "utf8"));
  if (expectations.schema !== "aether.m9-03.golden.v1") {
    fail(`[DoD-2] expectations.schema 非法：${expectations.schema}`);
  }
  for (const case_ of expectations.cases ?? []) {
    checked.fixtureCases += 1;
    const lines = readFileSync(path.join(fixtureDir, case_.file), "utf8")
      .split(/\r?\n/)
      .filter((line) => line.trim().length > 0);
    if (!Number.isInteger(case_.line) || case_.line < 0 || case_.line >= lines.length) {
      fail(`[DoD-2] 用例 ${case_.id} 行号越界：${case_.line}`);
    }
  }
  for (const case_ of expectations.pre_initialize_cases ?? []) checked.fixtureCases += 1;
  if (checked.fixtureCases < 15) fail(`[DoD-2] golden 用例数 ${checked.fixtureCases} < 15`);
  const ids = (expectations.cases ?? []).map((case_) => case_.id);
  for (const required of [
    "params-omitted",
    "params-null",
    "params-empty-object",
    "params-payload-wrapper-rejected",
    "params-inner-dto-accepted",
    "initialize-handshake",
    "transport-parse-error",
    "transport-unknown-method",
    "transport-duplicate-id",
    "shutdown-accepted",
    "shutdown-rejects-new-requests",
  ]) {
    if (!ids.includes(required)) fail(`[DoD-2] expectations 缺少用例 ${required}`);
  }
  if (!expectations.pre_initialize_cases?.some((case_) => case_.id === "pre-initialize-business")) {
    fail("[DoD-2] 缺少预初始化业务请求用例");
  }
  if (!expectations.oversized?.limit_bytes === 2 * 1024 * 1024) {
    fail("[DoD-2] oversized.limit_bytes 必须是 2097152");
  }
}

// ===== DoD-3/5/7：进程级测试用例冻结 =====
const stdioTests = read("crates/aether-daemon/tests/m9_03_stdio.rs");
const requiredTests = [
  "initialize_shutdown_and_stdout_is_jsonl_only",
  "initialize_handshake_meets_hard_timeout_and_records_target",
  "daemon_status_notifications_follow_phase_transitions",
  "params_shapes_are_equivalent_and_payload_wrapper_is_rejected",
  "transport_errors_do_not_kill_the_daemon",
  "oversized_frame_only_fails_the_request",
  "pre_initialize_business_request_is_core_not_ready_then_initialize_works",
  "second_daemon_on_same_data_dir_exits_20_with_first_frame_error",
  "db_open_failed_is_fatal_exit_22",
  "applied_migration_checksum_mismatch_is_fatal_exit_22",
  "quick_check_failure_enters_read_only_safe_mode_without_exiting",
];
for (const name of requiredTests) {
  if (!stdioTests.includes(`fn ${name}(`)) fail(`[DoD-3/5/7] 缺少进程级用例 ${name}`);
}
checked.testCases = [...stdioTests.matchAll(/#\[test\]/g)].length;
if (checked.testCases < requiredTests.length) {
  fail(`[DoD-3/5/7] 进程级用例数 ${checked.testCases} < ${requiredTests.length}`);
}
if (!existsSync(path.join(root, "crates/aether-rpc/tests/golden_frames.rs"))) {
  fail("[DoD-2] 缺少 aether-rpc golden 回放测试");
}

// ===== DoD-6：CLI 与文档 =====
const daemonLib = read("crates/aether-daemon/src/lib.rs");
for (const flag of ["--stdio", "--version", "--help"]) {
  if (!daemonLib.includes(flag)) fail(`[DoD-6] CLI 缺少 ${flag}`);
}
if (!daemonLib.includes("AETHER_DATA_DIR")) fail("[DoD-6] 未声明 AETHER_DATA_DIR 测试目录");
const evidence = "docs/M9-03-证据.md";
if (!existsSync(path.join(root, evidence))) {
  fail(`[DoD-6] 缺少证据文档 ${evidence}`);
} else {
  const text = read(evidence);
  for (const section of ["DoD-1", "DoD-2", "DoD-3", "DoD-4", "DoD-5", "DoD-6", "DoD-7"]) {
    if (!text.includes(`| ${section} `)) fail(`[证据] ${evidence} 缺少 ${section} 对照行`);
  }
}

// ===== 汇总 =====
for (const message of notes) console.log(`[m9-03] 说明：${message}`);
if (failures.length > 0) {
  console.error(`[m9-03] FAIL（${failures.length} 项）`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}

console.log(
  `[m9-03] OK：rpc 源文件 ${checked.rpcSources}（叶子）· daemon 源文件 ${checked.daemonSources}` +
    `（无 tauri）· 传输码 12 · DaemonPhase 8+2 保留 · 退出码 0/10/20/21/22/23/24 ·` +
    ` fixture ${checked.fixtureLines} 行/${checked.fixtureCases} 用例 · 进程级用例 ${checked.testCases} ·` +
    ` 业务码 9 与生成物一致`,
);
console.log("AETHER_M9_03 PASS");
