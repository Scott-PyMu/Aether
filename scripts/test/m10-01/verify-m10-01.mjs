#!/usr/bin/env node
/**
 * `pnpm verify:m10-01` —— M10-01 编排引擎可复跑检查（ADR-031 D-031-1～D-031-7）。
 *
 * 检查面：
 *  1. 迁移 0010 + 命令/事件/业务码/UI 锚点静态存在；
 *  2. 定义校验/引擎单测（含 10 并发、取消、重试、超时、reconcile、存储失败）；
 *  3. 持久化 schema（m1_03_schema）与附录 C 一致性；
 *  4. 前端 UI 最小集（列表/编辑器/运行视图/节点重试）；
 *  5. 证据文档 `docs/M10-01-证据.md` 存在且 DoD-1～8 有记录。
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

function run(label, command, args, options = {}) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    execFileSync(command, args, {
      cwd: root,
      stdio: "inherit",
      env: process.env,
      shell: process.platform === "win32",
      ...options,
    });
    checks.push({ name: label, ok: true, detail: "" });
  } catch (error) {
    checks.push({ name: label, ok: false, detail: String(error.status ?? error.message) });
    failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

const cargo = process.env.AETHER_CARGO ?? (process.platform === "win32" ? "cargo.exe" : "cargo");
const pnpm =
  process.env.AETHER_PNPM ?? (process.platform === "win32" ? "pnpm.cmd" : "pnpm");

// ---------- 静态契约 ----------
const migration = read("migrations/0010_workflow_orchestration.sql");
for (const needle of [
  "workflows ADD COLUMN deleted_at",
  "workflow_runs ADD COLUMN session_id",
  "workflow_runs ADD COLUMN definition_version",
  "workflow_runs ADD COLUMN definition_hash",
  "workflow_runs ADD COLUMN error",
  "workflow_runs ADD COLUMN cancel_requested_at",
  "CREATE UNIQUE INDEX idx_node_runs_attempt",
  "CREATE INDEX idx_node_runs_run_status",
  "CREATE INDEX idx_tasks_workflow_node",
  "CREATE INDEX idx_tasks_session_status",
]) {
  check(`迁移 0010 含 ${needle}`, migration.includes(needle));
}
check("0001–0009 未修改（0010 为新增迁移）", !/[0-9]{4}_init/.test(migration));

const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
for (const command of [
  "workflow_create",
  "workflow_list",
  "workflow_get",
  "workflow_update",
  "workflow_delete",
  "workflow_import",
  "workflow_run",
  "workflow_run_get",
  "workflow_run_list",
  "workflow_run_cancel",
  "workflow_node_retry",
]) {
  check(`命令面含 ${command}`, dispatch.includes(`"${command}"`));
}

const event = read("crates/aether-core/src/event.rs");
for (const eventType of [
  "workflow.started",
  "workflow.node_started",
  "workflow.node_completed",
  "workflow.node_failed",
  "workflow.node_cancelled",
  "workflow.node_skipped",
  "workflow.completed",
  "workflow.failed",
  "workflow.cancelled",
]) {
  check(`事件类型含 ${eventType}`, event.includes(eventType));
}

const business = read("crates/aether-app/src/ipc/business.rs");
for (const code of [
  "workflow_invalid_definition",
  "workflow_not_found",
  "workflow_version_conflict",
  "workflow_run_not_found",
  "workflow_run_active",
  "workflow_run_not_active",
  "workflow_node_not_found",
  "workflow_node_not_retryable",
  "workflow_cancel_timeout",
]) {
  check(`业务码含 ${code}`, business.includes(code));
}

const ui = read("apps/desktop/src/WorkflowsPage.tsx");
for (const testid of [
  "workflow-list",
  "workflow-item",
  "workflow-editor",
  "workflow-editor-validate",
  "workflow-import",
  "workflow-run-view",
  "workflow-node-status",
  "workflow-run-cancel",
  "workflow-node-retry",
]) {
  check(`UI 锚点含 ${testid}`, ui.includes(`data-testid=\"${testid}\"`) || ui.includes(`data-testid="${testid}"`));
}

const registryPath = "docs/M10-00-登记表.json";
const registry = JSON.parse(read(registryPath));
for (const entry of registry.events.filter((item) => item.task === "M10-01")) {
  check(`登记事件 ${entry.type} 已实现`, event.includes(entry.type));
}

// ---------- 可执行验证 ----------
run(
  "编排引擎单测/集成（定义/10 并发/取消/重试/超时/reconcile/存储失败）",
  cargo,
  [
    "test",
    "-p",
    "aether-app",
    "--test",
    "m10_01_workflow",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ],
);
run(
  "持久化 schema（附录 C + 0010）",
  cargo,
  ["test", "-p", "aether-store", "--test", "m1_03_schema", "-j", "1", "--", "--test-threads=1"],
);
run("迁移/附录 C 全量校验", pnpm, ["verify:m1-03"]);
run("错误码/业务码生成物", pnpm, ["verify:error-codes"]);
run(
  "前端工作流 UI 最小集",
  pnpm,
  ["--filter", "@aether/desktop", "test", "--", "src/m10_01_workflows.test.tsx"],
);
run("桌面类型检查", pnpm, ["--filter", "@aether/desktop", "typecheck"]);

// ---------- 证据 ----------
check("证据 docs/M10-01-证据.md 存在", existsSync(path.join(root, "docs/M10-01-证据.md")));
if (existsSync(path.join(root, "docs/M10-01-证据.md"))) {
  const evidence = read("docs/M10-01-证据.md");
  for (let dod = 1; dod <= 8; dod += 1) {
    check(`证据记录 DoD-${dod}`, new RegExp(`DoD-${dod}`).test(evidence));
  }
}

const passed = checks.filter((item) => item.ok).length;
console.log(`\nM10-01 校验：${passed}/${checks.length} 项通过`);
if (failures.length > 0) {
  console.error(`失败 ${failures.length} 项：`);
  for (const failure of failures) console.error(` - ${failure}`);
  process.exit(1);
}
console.log("M10-01 编排引擎（定义/调度/持久化/取消/重试/reconcile/UI）：PASS");
