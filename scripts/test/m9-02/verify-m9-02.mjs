#!/usr/bin/env node
/**
 * `pnpm verify:m9-02`（P2 计划 M9-02；ADR-029 D-029-2/D-029-4）。
 *
 * 可执行校验口径：
 *  1. `aether-app` 无 Tauri/窗口实现依赖：`Cargo.toml` 无 tauri/wry/webkit；
 *     `crates/aether-app/src/**` 代码行无 `use tauri` / `tauri::`（注释除外）；
 *  2. `aether-app::COMMANDS` 与 M9-01 HEAD 基线（`docs/M9-01-baseline/manifest.json`）
 *     63 条逐项一致（顺序 + 名称），且每条都有 dispatch 路由 arm；
 *  3. `aether-tauri` 薄适配：G8 前 63 个 wrapper 各调用一次
 *     `state.dispatch("<name>")`，不复制业务/校验逻辑（无 dto/validate/parse_* 引用），
 *     `collect_commands!` 顺序与 `COMMANDS` 一致；**G8/M9-14 后 crate 已退役**，
 *     改为断言退役事实（crate/workspace/lock/Tauri 引用清零），不得回退；
 *  4. 测试迁移：`aether-app/tests/ipc_validation.rs` 存在（矩阵 + 63 命令路由用例）；
 *     Tauri 侧测试在退役前保留（适配层 mock invoke 冒烟），退役后由 app 矩阵与
 *     M9-14 退役证据承接；
 *  5. 依赖方向写入 AGENTS（core → … → app → daemon；G8 后不再包含 tauri），
 *     workspace 成员与 `aether-app` 依赖声明落地。
 *
 * 动态证据（`cargo test` / `clippy` / `deny` / `cargo tree`）见 `docs/M9-02-证据.md`。
 */
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checked = { commands: 0, wrappers: 0, sourceFiles: 0 };

const COMMANDS_SRC = "crates/aether-app/src/ipc/dispatch.rs";
const TAURI_CRATE = "crates/aether-tauri";
const TAURI_COMMANDS_SRC = "crates/aether-tauri/src/ipc/commands.rs";
const APP_MANIFEST = "crates/aether-app/Cargo.toml";
const TAURI_MANIFEST = "crates/aether-tauri/Cargo.toml";
const WORKSPACE_MANIFEST = "Cargo.toml";
const CARGO_LOCK = "Cargo.lock";
const APP_TESTS = "crates/aether-app/tests/ipc_validation.rs";
const TAURI_TESTS = "crates/aether-tauri/tests/ipc_validation.rs";
const BASELINE_MANIFEST = "docs/M9-01-baseline/manifest.json";
const BASELINE_COMMANDS_DIR = "docs/M9-01-baseline/commands";
const AGENTS = "AGENTS.md";
const RETIREMENT_EVIDENCE = "docs/M9-14-证据.md";

const SHELL_COMMANDS = ["app_exit", "app_restart", "ref_pick", "startup_pick_target"];
const RESERVED_COMMANDS = ["startup_get", "startup_migrate"];

function fail(message) {
  failures.push(message);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

const workspace = read(WORKSPACE_MANIFEST);
const tauriRetired = !existsSync(path.join(root, TAURI_CRATE));

/** 去掉行注释与块注释（本仓库源码字符串内无 `//` 形态，简单处理足够）。 */
function stripComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
}

function extractStringArray(source, constName) {
  const block = source.match(
    new RegExp(`pub const ${constName}: &\\[&str\\] = &\\[([\\s\\S]*?)\\n\\];`),
  );
  if (!block) throw new Error(`未找到 ${constName} 常量块`);
  return [...block[1].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}

function rustSources(dir) {
  const result = [];
  const visit = (relative) => {
    const base = path.join(root, relative);
    for (const name of readdirSync(base)) {
      const nested = path.join(relative, name);
      const full = path.join(root, nested);
      if (statSync(full).isDirectory()) {
        visit(nested);
      } else if (name.endsWith(".rs")) {
        result.push(nested);
      }
    }
  };
  visit(dir);
  return result;
}

// ===== DoD-1：aether-app 无 Tauri 依赖 =====
const appManifest = stripComments(read(APP_MANIFEST));
for (const dep of ["tauri", "wry", "webkit2gtk", "webkit"]) {
  if (new RegExp(`^\\s*${dep}\\s*=`, "m").test(appManifest)) {
    fail(`[DoD-1] ${APP_MANIFEST} 不得依赖 ${dep}`);
  }
}

for (const rel of rustSources("crates/aether-app/src")) {
  const source = stripComments(read(rel));
  checked.sourceFiles += 1;
  if (/\buse tauri\b/.test(source)) fail(`[DoD-1] ${rel} 含 use tauri`);
  if (/\btauri::/.test(source)) fail(`[DoD-1] ${rel} 含 tauri:: 调用`);
}

// ===== DoD-2：COMMANDS = 基线 63 条，且路由 arm 齐备 =====
const dispatchSource = read(COMMANDS_SRC);
const commands = extractStringArray(dispatchSource, "COMMANDS");
checked.commands = commands.length;
const baseline = readJson(BASELINE_MANIFEST);
const baselineCommands = baseline.commands ?? [];
if (commands.length !== 63) fail(`[DoD-2] COMMANDS 数量 ${commands.length} ≠ 63`);
if (baseline.counts?.commands !== 63) {
  fail(`[DoD-2] 基线计数 commands=${baseline.counts?.commands} ≠ 63`);
}
if (commands.length !== baselineCommands.length) {
  fail(`[DoD-2] COMMANDS 与基线长度不一致：${commands.length} vs ${baselineCommands.length}`);
}
for (let index = 0; index < Math.max(commands.length, baselineCommands.length); index += 1) {
  if (commands[index] !== baselineCommands[index]) {
    fail(
      `[DoD-2] 第 ${index + 1} 条命令不一致：app=${commands[index] ?? "<缺失>"} baseline=${baselineCommands[index] ?? "<缺失>"}`,
    );
  }
}
if (new Set(commands).size !== commands.length) fail("[DoD-2] COMMANDS 存在重复命令名");
const shellPresent = SHELL_COMMANDS.filter((name) => commands.includes(name));
if (shellPresent.length !== SHELL_COMMANDS.length) {
  fail(`[DoD-2] 壳命令集合不齐备：${SHELL_COMMANDS.filter((name) => !commands.includes(name))}`);
}
if (baseline.counts?.daemon !== 59 || baseline.counts?.shell !== 4) {
  fail(
    `[DoD-2] 基线路由计数 daemon=${baseline.counts?.daemon} shell=${baseline.counts?.shell}（期望 59/4）`,
  );
}
for (const reserved of RESERVED_COMMANDS) {
  if (!commands.includes(reserved)) fail(`[DoD-2] 保留命令 ${reserved} 不在 63 内`);
}
for (const command of commands) {
  if (!dispatchSource.includes(`"${command}" => handlers::`)) {
    fail(`[DoD-2] ${command} 缺少 dispatch 路由 arm`);
  }
  const prefix = String(commands.indexOf(command) + 1).padStart(2, "0");
  const rel = path.join(BASELINE_COMMANDS_DIR, `${prefix}-${command}.json`);
  if (!existsSync(path.join(root, rel))) fail(`[DoD-2] 基线条目缺失 ${rel}`);
}
if (!dispatchSource.includes("other => return Err(unknown_method(other))")) {
  fail("[DoD-2] dispatch 缺少未知方法兜底（not_implemented）");
}

// ===== DoD-3：Tauri 薄适配（G8 前）/ 退役事实（G8 后） =====
if (tauriRetired) {
  // M9-14/G8：Tauri 已退役，禁止回退。原先的 63 wrapper / collect_commands!
  // 检查由「crate 不存在 + workspace/lock 清零 + app 命令面完整」承接。
  if (existsSync(path.join(root, TAURI_COMMANDS_SRC))) {
    fail(`[DoD-3] ${TAURI_CRATE} 已退役但 ${TAURI_COMMANDS_SRC} 仍存在`);
  }
  if (/"crates\/aether-tauri"/.test(workspace) || /^aether-tauri\s*=/m.test(workspace)) {
    fail("[DoD-3] G8 后 workspace 不得再包含/声明 aether-tauri");
  }
  if (/^name = "aether-tauri"$/m.test(read(CARGO_LOCK))) {
    fail("[DoD-3] G8 后 Cargo.lock 不得包含 aether-tauri");
  }
  if (!existsSync(path.join(root, RETIREMENT_EVIDENCE))) {
    fail(`[DoD-3] 缺少 Tauri 退役证据 ${RETIREMENT_EVIDENCE}`);
  } else {
    const retirement = read(RETIREMENT_EVIDENCE);
    for (const token of ["aether-tauri", "退役", "无 Tauri 回退包"]) {
      if (!retirement.includes(token)) {
        fail(`[DoD-3] ${RETIREMENT_EVIDENCE} 缺少退役标记「${token}」`);
      }
    }
  }
  checked.wrappers = 0;
} else {
  const tauriCommands = read(TAURI_COMMANDS_SRC);
  const dispatchCalls = [...tauriCommands.matchAll(/state\.dispatch\("([^"]+)"/g)].map(
    (match) => match[1],
  );
  checked.wrappers = dispatchCalls.length;
  if (dispatchCalls.length !== 63) {
    fail(`[DoD-3] tauri wrapper 的 state.dispatch 调用数 ${dispatchCalls.length} ≠ 63`);
  }
  for (const command of commands) {
    const count = dispatchCalls.filter((name) => name === command).length;
    if (count !== 1) fail(`[DoD-3] ${command} wrapper 的 dispatch 调用数 ${count} ≠ 1`);
  }
  for (const forbidden of ["parse_strict", "parse_no_params", "dto::", "validate::"]) {
    if (tauriCommands.includes(forbidden)) {
      fail(`[DoD-3] ${TAURI_COMMANDS_SRC} 含业务/校验逻辑引用 ${forbidden}`);
    }
  }
  const collectBlock = tauriCommands.match(/collect_commands!\[([\s\S]*?)\n\s*\]/);
  if (!collectBlock) {
    fail("[DoD-3] 未找到 tauri_specta::collect_commands! 声明");
  } else {
    const declared = [...collectBlock[1].matchAll(/^\s*([a-z_]+),/gm)].map((match) => match[1]);
    if (declared.length !== 63) fail(`[DoD-3] collect_commands! 数量 ${declared.length} ≠ 63`);
    for (let index = 0; index < commands.length; index += 1) {
      if (declared[index] !== commands[index]) {
        fail(
          `[DoD-3] collect_commands! 第 ${index + 1} 项 ${declared[index] ?? "<缺失>"} 与 COMMANDS ${commands[index] ?? "<缺失>"} 不一致`,
        );
      }
    }
  }
}

// ===== DoD-4：测试迁移 =====
if (!existsSync(path.join(root, APP_TESTS))) fail(`[DoD-4] 缺少 ${APP_TESTS}`);
const appTests = read(APP_TESTS);
const appTestCount = [...appTests.matchAll(/#\[test\]/g)].length;
if (appTestCount < 15) fail(`[DoD-4] ${APP_TESTS} 测试数 ${appTestCount} < 15`);
for (const required of [
  "fn every_command_is_routed_and_rejects_unknown_members",
  "fn commands_manifest_matches_frozen_baseline",
]) {
  if (!appTests.includes(required)) fail(`[DoD-4] 缺少用例 ${required}`);
}
if (tauriRetired) {
  if (existsSync(path.join(root, "crates/aether-tauri/tests"))) {
    fail("[DoD-4] G8 后不得保留 aether-tauri 测试目录");
  }
} else {
  if (!existsSync(path.join(root, TAURI_TESTS))) fail(`[DoD-4] 缺少适配层冒烟 ${TAURI_TESTS}`);
  const tauriTests = read(TAURI_TESTS);
  if (!tauriTests.includes("mock_builder") || !tauriTests.includes("get_ipc_response")) {
    fail("[DoD-4] Tauri ipc_validation 未保留 mock invoke 冒烟");
  }
}

// ===== DoD-5：依赖方向 =====
if (!workspace.includes('"crates/aether-app"')) {
  fail("[DoD-5] workspace 未包含 crates/aether-app");
}
if (tauriRetired) {
  if (workspace.includes('"crates/aether-tauri"')) {
    fail("[DoD-5] G8 后 workspace 不得包含 crates/aether-tauri");
  }
} else if (!/^aether-app\.workspace\s*=\s*true/m.test(stripComments(read(TAURI_MANIFEST)))) {
  fail("[DoD-5] aether-tauri 未声明 aether-app 依赖");
}
const agents = read(AGENTS);
if (!agents.includes("core → store / adapters / control / security → app → daemon")) {
  fail("[DoD-5] AGENTS §2.1 未写入 core → … → app → daemon 依赖方向");
}
if (!agents.includes("`aether-app` 不依赖 Tauri、Electron、Node")) {
  fail("[DoD-5] AGENTS §2.1 未写入 aether-app 禁依赖条款");
}
if (tauriRetired && !agents.includes("重新引入 `aether-tauri`")) {
  fail("[DoD-5] AGENTS §2.1 未写入禁止重新引入 aether-tauri（G8）");
}

// ===== 汇总 =====
if (failures.length > 0) {
  console.error(`[m9-02] FAIL（${failures.length} 项）`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}

console.log(
  `[m9-02] OK：命令 ${checked.commands}（59 daemon + 4 shell）· ` +
    (tauriRetired ? "Tauri 已退役（G8，无回退包）" : `wrapper ${checked.wrappers}`) +
    ` · app 源文件 ${checked.sourceFiles}（无 tauri）· 基线一致 · 测试迁移齐备`,
);
