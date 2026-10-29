#!/usr/bin/env node
/**
 * `pnpm verify:m9-06`（P2 计划 M9-06；ADR-029 D-029-6 / D-029-9 / §4.4 / §4.5 / §4.6）。
 *
 * 可执行校验（静态 + 动态；动态证据见 `docs/M9-06-证据.md`）：
 *  1. DoD-1：适配器权限回环 E2E（allow/deny/300s 超时/取消）、审计、零直通；
 *  2. DoD-2：pause/resume/interrupt/run_retry 语义与 `pause_unsupported` 能力门；
 *  3. DoD-3：`ref_pick`/`startup_pick_target` 壳本地（严格参数/canonical/取消）
 *     与 `artifact_add` 复核；
 *  4. DoD-4：server-initiated `dialog.pick` 仅协议 fixture（正常/超时/取消/迟到丢弃），
 *     产品路径不引用；59/4 不变；
 *  5. DoD-5：`permissions_pending` 轮询 + `permission.pending` 通知并存，唯一决议
 *     入口 `permission_resolve`。
 */
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, result, expect = 0) =>
  checks.push({
    name,
    exit: typeof result === "boolean" ? (result ? 0 : 1) : result,
    expect,
  });

function read(rel) {
  return readFileSync(path.join(repoRoot, rel), "utf8");
}

/** 去掉注释后再做「产品路径不得引用」类静态检查（注释中的协议说明不算接线）。 */
function sourceWithoutComments(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(repoRoot, rel));
}

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const pnpm = pnpmCommand();
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-06");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);

// ===== 1. Mock 适配器（真实回环宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；M9-06 真实回环宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }) === 0,
);

// ===== 2. Rust 动态测试 =====

record(
  "aether-rpc：dialog.pick 协议预留单测（正常/超时/取消/迟到丢弃/严格形状）",
  run(cargo, ["test", "-p", "aether-rpc", "--lib", "server_request"]) === 0,
);
record(
  "aether-rpc：dialog.pick fixture 回放（scripts/test/m9-06/fixtures/dialog-pick.json）",
  run(cargo, ["test", "-p", "aether-rpc", "--test", "m9_06_dialog_pick"]) === 0,
);
record(
  "aether-app：permission.pending 通知桥（仅提示；lag/计数/出口失败隔离）",
  run(cargo, ["test", "-p", "aether-app", "--test", "m9_06_permission_notice"]) === 0,
);
record(
  "aether-app：权限回环 E2E（allow/deny/300s/取消/pause/resume/run_retry/artifact_add）",
  run(
    cargo,
    ["test", "-p", "aether-app", "--test", "m9_06_permission_bridge", "--", "--test-threads=1"],
    {
      env: {
        AETHER_MOCK_ADAPTER: mockAdapter,
        AETHER_REQUIRE_MOCK_ADAPTER: "1",
      },
    },
  ) === 0,
);
record(
  "aether-daemon：permission.pending 进程级通知 + 唯一决议路径 + 审计落库",
  run(cargo, ["test", "-p", "aether-daemon", "--test", "m9_06_permission_pending"]) === 0,
);

// ===== 3. Electron 壳本地选择器 =====

record(
  "desktop：ref_pick/startup_pick_target 壳本地单测（文件/目录/取消/非法路径/canonical）",
  run(pnpm.command, [
    ...pnpm.prefix,
    "--filter",
    "@aether/desktop",
    "test",
    "electron/localCommands.test.ts",
  ]) === 0,
);

// ===== 4. 静态契约 =====

const problems = [];
const fail = (message) => problems.push(message);

// DoD-4：dialog.pick 仅协议预留（产品代码 0 引用）。
const productFiles = [
  "crates/aether-app/src/permission_notice.rs",
  "crates/aether-app/src/permission_loop.rs",
  "crates/aether-app/src/adapter_executor.rs",
  "crates/aether-app/src/session_backend.rs",
  "crates/aether-app/src/ipc/dispatch.rs",
  "crates/aether-daemon/src/serve.rs",
  "apps/desktop/src/permission.ts",
  "apps/desktop/electron/localCommands.ts",
];
for (const rel of productFiles) {
  if (!exists(rel)) {
    fail(`[DoD-4] 缺少产品文件 ${rel}`);
    continue;
  }
  if (sourceWithoutComments(read(rel)).includes("dialog.pick")) {
    fail(`[DoD-4] 产品路径不得引用 dialog.pick：${rel}`);
  }
}
const rpcSource = read("crates/aether-rpc/src/server_request.rs");
for (const needle of [
  'pub const DIALOG_PICK_METHOD: &str = "dialog.pick";',
  "pub const SERVER_REQUEST_TIMEOUT_MS: u64 = 300_000;",
  "pub const DIALOG_PICK_TIMEOUT_MS: u64 = 120_000;",
  "pub fn expire(",
  "pub fn cancel(",
  "late_dropped",
]) {
  if (!rpcSource.includes(needle)) fail(`[DoD-4] server_request 缺少 ${needle}`);
}

// 59/4：dialog.pick 不在命令面；壳本地仍为 4 条。
const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
const shellBlock = dispatch.match(/pub const SHELL_COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/);
if (!shellBlock) {
  fail("[DoD-4] 无法解析 SHELL_COMMANDS");
} else {
  const shell = [...shellBlock[1].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
  if (shell.join(",") !== "app_exit,app_restart,ref_pick,startup_pick_target") {
    fail(`[DoD-4] 壳本地命令漂移：${shell.join(",")}`);
  }
}
if (/^\s*"dialog\.pick",/m.test(dispatch)) {
  fail("[DoD-4] dialog.pick 不得进入 63 条命令清单");
}
const manifest = readJson("packages/protocol/src/command-manifest.json");
if (manifest.counts?.total !== 63 || manifest.counts?.daemon !== 59 || manifest.counts?.shell !== 4) {
  fail(
    `[DoD-4] 路由计数漂移：total=${manifest.counts?.total} daemon=${manifest.counts?.daemon} shell=${manifest.counts?.shell}`,
  );
}

// DoD-5：通知只带 count/session_id；不出现决议调用（唯一入口 permission_resolve）。
const notice = read("crates/aether-app/src/permission_notice.rs");
for (const needle of [
  'pub const PERMISSION_PENDING_METHOD: &str = "permission.pending";',
  "EventType::PermissionRequested",
  "EventType::PermissionResolved",
])
  if (!notice.includes(needle)) fail(`[DoD-5] permission_notice 缺少 ${needle}`);
if (/permission_resolve|permission\.resolve/.test(sourceWithoutComments(notice))) {
  fail("[DoD-5] 通知桥不得出现决议调用（唯一入口 permission_resolve）");
}
const serve = read("crates/aether-daemon/src/serve.rs");
for (const needle of [
  "PermissionNoticeBridge::new(",
  "permission_pending_notification",
  'PERMISSION_PENDING_METHOD',
]) {
  if (!serve.includes(needle)) fail(`[DoD-5] daemon 未接线 ${needle}`);
}
const noticeStop = serve.indexOf("权限通知桥已停止");
const bridgeStop = serve.indexOf("事件桥已停止");
if (noticeStop < 0 || bridgeStop < 0 || noticeStop > bridgeStop) {
  fail("[DoD-5] 关闭顺序必须是先停通知桥、再停事件桥");
}

// DoD-2：暂停能力门与取消树接线仍在。
const appPermissionLoop = read("crates/aether-app/src/permission_loop.rs");
if (!appPermissionLoop.includes("request_cancellable")) {
  fail("[DoD-2] 权限网关未绑定取消树（M2-05 权限等待可取消）");
}
const executor = read("crates/aether-app/src/adapter_executor.rs");
for (const needle of ["cancel_tokens", "CancelTokenGuard", "decide_with_cancel"]) {
  if (!executor.includes(needle)) fail(`[DoD-2] 执行器缺少 ${needle}`);
}
const sessionBackend = read("crates/aether-app/src/session_backend.rs");
if (!sessionBackend.includes("ensure_pause_supported")) {
  fail("[DoD-2] pause_unsupported 能力门缺失");
}

// DoD-3：壳本地选择器严格参数 + canonical + 后续 artifact_add 复核。
const local = read("apps/desktop/electron/localCommands.ts");
for (const needle of [
  'export const REF_PICK_METHOD = "ref_pick";',
  'export const STARTUP_PICK_TARGET_METHOD = "startup_pick_target";',
  "realpathSync.native",
  '"unknown_field"',
  '"invalid_enum"',
  '"path_rejected"',
  "openDirectory",
]) {
  if (!local.includes(needle)) fail(`[DoD-3] localCommands 缺少 ${needle}`);
}
if (!read("crates/aether-app/src/ipc/dto.rs").includes("fn resolve_path")) {
  fail("[DoD-3] artifact_add 未复核选择器路径");
}

// 渲染层：决议只经 permission_resolve；通知不参与决议。
const rendererPermission = read("apps/desktop/src/permission.ts");
if (!rendererPermission.includes('"permission_resolve"')) {
  fail("[DoD-5] renderer 未使用 permission_resolve");
}
if (read("apps/desktop/src/permissionProjection.ts").includes("permission_resolve")) {
  fail("[DoD-5] 权限投影不得自行决议（只做清单校准）");
}

// 测试用例名冻结（可复跑证据）。
const bridgeTest = read("crates/aether-app/tests/m9_06_permission_bridge.rs");
for (const name of [
  "permission_loop_allow_flows_from_adapter_to_ui_and_back",
  "permission_loop_deny_is_delivered_to_the_adapter",
  "permission_loop_timeout_denies_at_three_hundred_seconds",
  "permission_loop_cancel_and_run_retry_keep_channel_semantics",
  "pause_resume_and_capability_gate_are_unchanged",
  "picked_paths_are_rechecked_by_artifact_add",
]) {
  if (!bridgeTest.includes(`fn ${name}(`)) fail(`[DoD-1/2/3] 缺少用例 ${name}`);
}
const daemonTest = read("crates/aether-daemon/tests/m9_06_permission_pending.rs");
if (!daemonTest.includes("permission_pending_notification_and_single_resolution_path")) {
  fail("[DoD-5] 缺少 daemon 进程级通知/决议用例");
}
const noticeTest = read("crates/aether-app/tests/m9_06_permission_notice.rs");
for (const name of [
  "permission_events_trigger_hint_notices_only",
  "lagged_broadcast_resyncs_with_global_count",
  "count_and_sink_failures_are_isolated",
]) {
  if (!noticeTest.includes(`fn ${name}(`)) fail(`[DoD-5] 缺少通知桥用例 ${name}`);
}
const localTest = read("apps/desktop/electron/localCommands.test.ts");
for (const name of [
  "ref_pick 文件返回 canonical 路径",
  "ref_pick 目录与取消语义",
  "startup_pick_target 仍是通用目录选择器",
  "参数严格解析",
  "非法路径",
]) {
  if (!localTest.includes(name)) fail(`[DoD-3] 缺少选择器用例 ${name}`);
}

// fixture 与常量同源。
const fixture = readJson("scripts/test/m9-06/fixtures/dialog-pick.json");
if (fixture.schema !== "aether.m9-06.dialog-pick.v1") fail("[DoD-4] fixture schema 非法");
if (fixture.method !== "dialog.pick") fail("[DoD-4] fixture method 非法");
if (fixture.timeouts_ms?.default !== 300_000 || fixture.timeouts_ms?.dialog !== 120_000) {
  fail("[DoD-4] fixture 超时与 ADR-029 §4.5 不一致");
}
for (const key of [
  "request",
  "normal_response",
  "cancel_response",
  "timeout_request",
  "late_response",
  "invalid_params",
  "invalid_result",
]) {
  if (!(key in (fixture.cases ?? {}))) fail(`[DoD-4] fixture 缺少用例 ${key}`);
}

// 脚本/证据登记。
const pkg = readJson("package.json");
if (pkg.scripts?.["verify:m9-06"] !== "node scripts/test/m9-06/verify-m9-06.mjs") {
  fail("[证据] package.json 缺少 verify:m9-06");
}
if (!exists("docs/M9-06-证据.md")) {
  fail("[证据] 缺少 docs/M9-06-证据.md");
} else {
  const evidence = read("docs/M9-06-证据.md");
  for (const row of ["DoD-1", "DoD-2", "DoD-3", "DoD-4", "DoD-5"]) {
    if (!evidence.includes(`| ${row} `)) fail(`[证据] 缺少 ${row} 对照行`);
  }
}

if (problems.length > 0) {
  console.error("[m9-06] 静态检查失败：");
  for (const problem of problems) console.error(`  - ${problem}`);
}
record("静态契约：dialog.pick 仅预留 / 59+4 / 通知唯一决议路径 / 选择器复核在案", problems.length === 0 ? 0 : 1);

const exit = summarize("verify-m9-06", checks);
if (exit === 0) {
  console.log("[m9-06] PASS 权限回环 allow/deny/300s/取消 · 暂停能力门 · 选择器 canonical · dialog.pick 仅 fixture");
  console.log("AETHER_M9_06 PASS");
}
process.exit(exit);
