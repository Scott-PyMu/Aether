#!/usr/bin/env node
/**
 * `pnpm verify:m10-02` —— 子 Agent 派生与结果回注（ADR-032 v1.2）。
 *
 * 检查面：
 *  1. 迁移 0011（结果/注入/索引）与命令/事件/业务码静态登记；
 *  2. Rust 集成测试（派生/隔离/权限/拦截/父授权/取消）；
 *  3. 适配器 SDK 协议客户端与工具定义；
 *  4. UI-03 子会话树/结果卡锚点与交互；
 *  5. 生成物（79 命令 / 24 业务码）与 M9-01 基线；
 *  6. 证据文档 `docs/M10-02-证据.md` DoD-1～8。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
const failures = [];

function check(name, condition, detail = "") {
  checks.push({ name, ok: Boolean(condition), detail });
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function run(label, command, args) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    execFileSync(command, args, {
      cwd: root,
      stdio: "inherit",
      env: process.env,
      shell: process.platform === "win32",
    });
    checks.push({ name: label, ok: true, detail: "" });
  } catch (error) {
    checks.push({ name: label, ok: false, detail: String(error.status ?? error.message) });
    failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

const cargo = process.env.AETHER_CARGO ?? (process.platform === "win32" ? "cargo.exe" : "cargo");
const pnpm = process.env.AETHER_PNPM ?? (process.platform === "win32" ? "pnpm.cmd" : "pnpm");

// ---------- 静态契约 ----------
const migration = read("migrations/0011_subagent_results.sql");
for (const needle of [
  "idx_sessions_parent_status",
  "CREATE TABLE subagent_results",
  "CREATE TABLE subagent_injections",
  "supersedes_id",
  "DEFAULT 'message_append'",
  "idx_subagent_injections_ctx",
]) {
  check(`迁移 0011 含 ${needle}`, migration.includes(needle));
}

const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
for (const command of [
  "subagent_spawn",
  "subagent_list",
  "subagent_cancel",
  "subagent_resolve",
  "subagent_result_get",
]) {
  check(`命令面含 ${command}`, dispatch.includes(`"${command}"`));
}
check("命令面 79 条（75 daemon + 4 shell）", /COMMANDS\.len\(\), 79/.test(dispatch));

const event = read("crates/aether-core/src/event.rs");
for (const needle of [
  "subagent.spawned",
  "subagent.completed",
  "parent_session_id",
  "context_refs_count",
  "batch_id",
  "injected",
]) {
  check(`事件契约含 ${needle}`, event.includes(needle));
}

const business = read("crates/aether-app/src/ipc/business.rs");
for (const code of [
  "subagent_not_found",
  "subagent_limit_exceeded",
  "subagent_context_rejected",
  "subagent_result_not_ready",
  "subagent_resolution_conflict",
  "session_has_children",
]) {
  check(`业务码含 ${code}`, business.includes(code));
}

const backend = read("crates/aether-app/src/session_backend.rs");
check("session_delete 有子会话拒绝", backend.includes("SessionHasChildren"));
check("session_list 支持 include_children", backend.includes("include_children"));
check("会话摘要含 has_children", backend.includes("has_children"));

const control = read("crates/aether-app/src/subagent/control.rs");
for (const needle of [
  "SUBAGENT_DEPTH_MAX",
  "SUBAGENT_FANOUT_MAX",
  "SUBAGENT_CONCURRENCY_MAX",
  "subagent.result.injected",
  "origin_fork_unavailable",
  "message_append",
]) {
  check(`子 Agent 控制层含 ${needle}`, control.includes(needle));
}

const sdk = read("packages/adapter-sdk/src/subagent.ts");
for (const method of [
  "subagent.spawn",
  "subagent.cancel",
  "subagent.status",
  "subagent.result.get",
]) {
  check(`适配器 SDK 含 ${method}`, sdk.includes(method));
}
check("适配器 SDK 提供 subagent_spawn 工具", sdk.includes("aether_subagent_spawn"));

const ui = read("apps/desktop/src/SubagentPanel.tsx");
for (const testid of [
  "subagent-tree",
  "subagent-node",
  "subagent-result",
  "subagent-result-action",
]) {
  check(
    `UI 锚点含 ${testid}`,
    ui.includes(`data-testid="${testid}"`),
  );
}
check("UI 不新增 subagent-result-detail testid", !ui.includes('data-testid="subagent-result-detail"'));

const manifest = JSON.parse(read("packages/protocol/src/command-manifest.json"));
check("command-manifest 79 条", manifest.counts?.total === 79);
check("command-manifest daemon=75", manifest.counts?.daemon === 75);
const errors = JSON.parse(read("packages/protocol/src/error_codes.json"));
check("error_codes ipc=32", errors.ipc?.length === 32);
check("error_codes business=24", errors.business?.length === 24);

// ---------- 可执行验证 ----------
run(
  "子 Agent 集成（派生/隔离/权限/上限/父授权/处置/并行取消/失败/超时/reconcile）",
  cargo,
  [
    "test",
    "-p",
    "aether-app",
    "--test",
    "m10_02_subagent",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ],
);
run(
  "父会话删除拒绝（真实 SessionBackend）",
  cargo,
  [
    "test",
    "-p",
    "aether-app",
    "--test",
    "m10_02_parent_delete",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ],
);
run(
  "存储 schema/迁移（0011 + 附录 C）",
  cargo,
  ["test", "-p", "aether-store", "--test", "m1_03_schema", "-j", "1", "--", "--test-threads=1"],
);
run("命令清单生成物", "node", ["scripts/ci/command-manifest.mjs", "check"]);
run("错误码生成物", "node", ["scripts/ci/error-codes.mjs", "check"]);
run("客户端生成物", "node", ["scripts/ci/protocol-client.mjs", "check"]);
run("协议类型重新生成", "node", ["scripts/ci/protocol-types.mjs", "generate"]);
run("错误码/业务码", pnpm, ["verify:error-codes"]);
run("适配器 SDK 子 Agent 客户端", pnpm, ["--filter", "@aether/adapter-sdk", "test", "--", "src/subagent.test.ts"]);
run(
  "UI-03 子会话树/结果卡",
  pnpm,
  ["--filter", "@aether/desktop", "test", "--", "src/m10_02_subagents.test.tsx"],
);
run("桌面类型检查", pnpm, ["--filter", "@aether/desktop", "typecheck"]);
run("适配器 SDK 类型检查", pnpm, ["--filter", "@aether/adapter-sdk", "typecheck"]);

// ---------- 真实 CLI 交付模式（有本机二进制时执行） ----------
function hasCli(name) {
  try {
    execFileSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", `where ${name}`], {
      stdio: "ignore",
      env: process.env,
    });
    return true;
  } catch {
    return false;
  }
}
if (["claude", "codex", "dsh"].every(hasCli)) {
  run("真实三运行时 message_append/origin_fork（请求体断言）", "node", [
    "scripts/test/m10-02/real-delivery-spike.mjs",
  ]);
} else {
  console.log("NOTE 真实三运行时 CLI 缺失（claude/codex/dsh 任一），已跳过真实 CLI 交付模式 spike");
}

// ---------- 证据 ----------
check("证据 docs/M10-02-证据.md 存在", existsSync(path.join(root, "docs/M10-02-证据.md")));
if (existsSync(path.join(root, "docs/M10-02-证据.md"))) {
  const evidence = read("docs/M10-02-证据.md");
  for (let dod = 1; dod <= 8; dod += 1) {
    check(`证据记录 DoD-${dod}`, new RegExp(`DoD-${dod}`).test(evidence));
  }
  for (const section of ["隔离矩阵", "回注样本", "并行探索", "权限与审计", "UI-03"]) {
    check(`证据含「${section}」`, evidence.includes(section));
  }
}

const passed = checks.filter((item) => item.ok).length;
console.log(`\nM10-02 校验：${passed}/${checks.length} 项通过`);
if (failures.length > 0) {
  console.error(`失败 ${failures.length} 项：`);
  for (const failure of failures) console.error(` - ${failure}`);
  process.exit(1);
}
console.log("M10-02 子 Agent（派生/隔离/权限/回注/父授权/UI-03）：PASS");
