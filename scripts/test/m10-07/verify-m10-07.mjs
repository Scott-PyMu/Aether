#!/usr/bin/env node
/**
 * `pnpm verify:m10-07` —— 调度器完整路由（ADR-037）可复跑检查。
 *
 * 检查面：
 *  1. ADR-037 v1.0、迁移 0013（runs 5 列 + idx_runs_queue）；
 *  2. 命令面 +1（87/83/4；queue_list）；业务码 0 新增（35 不变）；
 *  3. 核心优先级/公平性/超时/退避/自动重试分类常量与实现；
 *  4. `session_send` 增量（priority/timeout_ms/max_attempts）与 `run_retry` 分类；
 *  5. `health.concurrency_gate` 扩展字段 + `queue_list` 后端映射；
 *  6. 重启保留 queued / 10 并发 / 超时 / 自动重试集成测试；
 *  7. UI 等待原因 + 重试入口（M7-02 现有锚点）；
 *  8. 证据文档 DoD-1～7；生成物 diff=0。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
const failures = [];

function check(name, condition, detail = "") {
  checks.push(name);
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function mustContain(rel, needles) {
  const text = read(rel);
  for (const needle of needles) {
    check(`${rel} 含 ${needle}`, text.includes(needle));
  }
}

function tool(name) {
  const override = process.env[`AETHER_${name.toUpperCase()}`];
  if (override) return override;
  if (name === "cargo") {
    const candidate = path.join(
      os.homedir(),
      ".rustup",
      "toolchains",
      "stable-x86_64-pc-windows-msvc",
      "bin",
      process.platform === "win32" ? "cargo.exe" : "cargo",
    );
    if (process.platform === "win32" && existsSync(candidate)) return candidate;
  }
  return name;
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
    checks.push(label);
  } catch (error) {
    checks.push(label);
    failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

// ---------- 1) ADR / 迁移 ----------
mustContain("docs/adr/ADR-037-m10-scheduler-priority-retry-routing.md", [
  "已接受",
  "v1.1",
  "fail-closed",
  "不自动重建队列",
  "score = base_priority",
  "wait_boost",
  "starvation_guard_ms=300000",
  "max_attempts",
  "queue_list",
  "runs.priority",
  "runs.queued_at",
  "runs.retry_count",
  "runs.max_attempts",
  "runs.wait_reason",
]);
mustContain("migrations/0013_scheduler_queue.sql", [
  "ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
  "ALTER TABLE runs ADD COLUMN queued_at INTEGER",
  "ALTER TABLE runs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
  "ALTER TABLE runs ADD COLUMN max_attempts INTEGER",
  "ALTER TABLE runs ADD COLUMN wait_reason TEXT",
  "CREATE INDEX idx_runs_queue ON runs(status, priority DESC, queued_at ASC)",
]);

// ---------- 2) 命令面 / 业务码 ----------
const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
check("COMMANDS = 87", /COMMANDS\.len\(\),\s*87/.test(dispatch));
check("daemon = 83", /daemon\.len\(\),\s*83/.test(dispatch));
check("命令面含 queue_list", dispatch.includes('"queue_list"'));
const manifest = readJson("packages/protocol/src/command-manifest.json");
check("command-manifest 87 条", manifest.counts?.total === 87, String(manifest.counts?.total));
check("command-manifest daemon=83", manifest.counts?.daemon === 83);
check("command-manifest shell=4", manifest.counts?.shell === 4);
check("command-manifest ui=85", manifest.counts?.ui === 85);
check(
  "scheduler 分组 = 1",
  manifest.groups?.some((group) => group.group === "scheduler" && group.count === 1),
);
const errorCodes = readJson("packages/protocol/src/error_codes.json");
check("business 码 = 35（0 新增）", errorCodes.business?.length === 35, String(errorCodes.business?.length));

// ---------- 3) 核心调度器 ----------
mustContain("crates/aether-control/src/gate.rs", [
  "QUEUE_PRIORITY_MIN: i32 = -100",
  "QUEUE_PRIORITY_MAX: i32 = 100",
  "MAX_QUEUED_GLOBAL: usize = 64",
  "WAIT_BOOST_STEP_MS: i64 = 10_000",
  "WAIT_BOOST_MAX: i32 = 60",
  "STARVATION_GUARD_MS: i64 = 300_000",
  "STARVATION_GUARD_PENALTY: i32 = 100",
  "DEFAULT_RUN_TIMEOUT_MS: i64 = 30 * 60 * 1000",
  "MIN_RUN_TIMEOUT_MS: i64 = 1_000",
  "MAX_RUN_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000",
  "MAX_ATTEMPTS_LIMIT: u32 = 5",
  "RETRY_BACKOFF_MS: [i64; 7]",
  "TIMEOUT_SETTLE_GRACE_MS: i64 = 5_000",
  "pub struct GateWaiter",
  "pub struct QueueSnapshotEntry",
  "starvation_guarded",
]);
mustContain("crates/aether-control/src/scheduler.rs", [
  "AUTO_RETRYABLE_CODES",
  "MANUAL_RETRY_BLOCKED_CODES",
  "backoff_with_jitter_ms",
  "manual_retry_blocked",
  "validate_priority",
  "validate_timeout_ms",
  "validate_max_attempts",
]);
mustContain("crates/aether-control/src/lifecycle.rs", [
  "pub async fn queue_list",
  "pub struct QueueEntry",
  "pub struct QueueListReport",
  "pub struct RunScheduleOptions",
  "pub async fn process_pending_retries_once",
  "pub async fn sweep_starvation_guard_once",
  "scheduler.starvation_guard",
  "run.timeout",
  "RUN_TIMEOUT_CODE",
  "不自动重放",
  "fail-closed",
  "unfinished_runs",
  "run_interrupted",
  "run.timeout",
  "finalize_timeout",
  "schedule_auto_retry",
  "runtime_loads",
  "session_active_priority",
]);
mustContain("crates/aether-control/src/routing.rs", [
  "active_runs",
  "equal_priority_prefers_lower_load_before_id",
]);

// ---------- 4) app 命令/健康 ----------
mustContain("crates/aether-app/src/ipc/dto.rs", [
  "pub priority: Option<i64>",
  "pub timeout_ms: Option<i64>",
  "pub max_attempts: Option<i64>",
  "pub struct QueueListRequest",
  "schedule_options",
]);
mustContain("crates/aether-app/src/session_backend.rs", [
  "fn queue_list",
  "send_with_schedule",
  "RetryNotAllowed",
  "runtime_loads",
]);
mustContain("crates/aether-app/src/ipc/dispatch.rs", ["queue_list"]);
mustContain("crates/aether-app/src/ipc/backend.rs", ["fn queue_list"]);
mustContain("crates/aether-app/src/ipc/handlers.rs", ["pub(crate) fn queue_list"]);
mustContain("crates/aether-core/src/domain.rs", ["RunStatus"]);

// ---------- 5) 迁移登记 / 设计附录 ----------
mustContain("crates/aether-store/src/migration.rs", [
  "version: 13",
  "0013_scheduler_queue.sql",
]);
if (existsSync(path.join(root, "设计文档.md"))) {
  mustContain("设计文档.md", [
    "ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
    "CREATE INDEX idx_runs_queue ON runs(status, priority DESC, queued_at ASC)",
  ]);
} else {
  checks.push("设计文档附录 C（跳过：本地冻结基线未入库）");
}

// ---------- 6) UI ----------
mustContain("apps/desktop/src/SessionWorkbench.tsx", [
  "data-wait-reason=\"concurrency_gate\"",
  "session.queue_wait_reason",
  "run-retry",
]);
mustContain("apps/desktop/src/health.ts", ["queued_by_priority", "starvation_guard"]);
mustContain("apps/desktop/src/i18n/locales/zh-CN.json", [
  "session.queue_wait_reason",
  "session.wait_reason.concurrency_gate",
]);
mustContain("apps/desktop/src/i18n/locales/en-US.json", [
  "session.queue_wait_reason",
  "session.wait_reason.concurrency_gate",
]);

// ---------- 7) 证据 ----------
check("证据文档存在", existsSync(path.join(root, "docs/M10-07-证据.md")));
if (existsSync(path.join(root, "docs/M10-07-证据.md"))) {
  mustContain("docs/M10-07-证据.md", [
    "DoD-1",
    "DoD-2",
    "DoD-3",
    "DoD-4",
    "DoD-5",
    "DoD-6",
    "DoD-7",
  ]);
}

// ---------- 8) 可执行验证 ----------
const cargo = tool("cargo");
run("control 调度器集成 7/7", cargo, [
  "test",
  "-p",
  "aether-control",
  "--test",
  "m10_07_scheduler",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("app IPC 校验 15/15", cargo, [
  "test",
  "-p",
  "aether-app",
  "--test",
  "ipc_validation",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("daemon 路由 83", cargo, [
  "test",
  "-p",
  "aether-daemon",
  "--test",
  "m9_04_routes",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("store 迁移/模式", cargo, [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m1_03_schema",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("UI 等待原因", "pnpm", [
  "--filter",
  "@aether/desktop",
  "test",
  "src/m7_02_queue_position.test.tsx",
]);
run("command-manifest diff=0", "node", ["scripts/ci/command-manifest.mjs", "check"]);
run("error-codes diff=0", "node", ["scripts/ci/error-codes.mjs", "check"]);
run("protocol-client diff=0", "node", ["scripts/ci/protocol-client.mjs", "check"]);
run("M9-01 基线", "node", ["scripts/test/verify-m9-01-baseline.mjs"]);
if (existsSync(path.join(root, "设计文档.md"))) {
  run("M1-03 schema", "node", ["scripts/test/m1-03/verify-m1-03.mjs"]);
} else {
  checks.push("M1-03 schema（跳过：本地冻结基线 设计文档.md 未入库）");
}

const passed = checks.length - failures.length;
if (failures.length > 0) {
  console.error(`\n[M10-07] FAIL：${failures.length} 项失败`);
  for (const failure of failures) console.error(` - ${failure}`);
  process.exit(1);
}
console.log(
  `\n[m10-07] OK：${passed}/${checks.length} 项通过（ADR-037 v1.1；87 命令 / 35 业务码；优先级/超时/重试/queue_list/fail-closed）`,
);
