/**
 * M2-05 验证入口：取消树与看门狗（设计 D8）。
 *
 * 覆盖 DoD：
 *   1) 取消风暴：20 会话并发 dispose → 全部 10s 内退出（集成；取消响应型执行器零 dump）；
 *   2) interrupt 后会话回 idle 且可续聊（API-E2E；含权限等待经取消树级联取消 +
 *      父会话 dispose 级联子/孙会话）；
 *   3) 不响应任务注入 → 恰好 10s 记录任务 dump + 强制清理；dump 进诊断缓冲
 *      （M3-05 诊断包消费）并经 tracing 上报（M2-07 日志汇聚端）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；控制/存储层用例跨平台。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");

// ===== 控制层单测：取消树级联 / 任务看门狗（10s 阈值/环形缓冲/出册）+ D8 常量 =====

record(
  "cargo test -p aether-control --lib（取消树/看门狗 + D8 10s 常量 + 生命周期配置）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);

// ===== DoD1–DoD3：集成（--nocapture 输出证据） =====

record(
  "m2_05_cancel（DoD1 取消风暴 20 会话 dispose ≤10s 零 dump；DoD2 interrupt→idle→续聊 + 权限等待级联取消 + 父取消级联；DoD3 10s dump+强制清理）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_05_cancel",
    "--",
    "--nocapture",
  ]),
);
record(
  "m2_05_watchdog_log（dump 的 tracing 上报捕获：M2-07 日志汇聚端输入）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_05_watchdog_log",
    "--",
    "--nocapture",
  ]),
);

// ===== 存储层：父取消级联查询（SessionQuery.parent_session_id） =====

record(
  "m2_01_domain_ops（含 M2-05 新增：sessions 按 parent_session_id 过滤）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m2_01_domain_ops"]),
);

// ===== 回归：M1-05 管线/降级 + M2-01 生命周期 + M2-03 权限 + M2-04 背压 =====

record(
  "回归 aether-control 全量（m1_05_* / m2_01_lifecycle / m2_03_permission / m2_04_backpressure）",
  run(cargo, ["test", "-p", "aether-control"]),
);

// ===== 静态检查：D8 取消树/看门狗/权限可取消接线在案 =====

{
  const problems = [];

  const cancel = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "cancel.rs"),
    "utf8",
  );
  const cancelRequired = [
    ["TASK_FORCE_CLEANUP_MS: i64 = 10_000", "D8 取消后 10s 强制清理"],
    ["TASK_DUMP_ACTION_FORCED_CLEANUP", "dump 动作标记"],
    ["pub struct CancelTree", "取消树"],
    ["pub struct TaskWatchdog", "任务看门狗"],
    ["child_token()", "父取消级联（tokio-util 子节点）"],
    ["pub fn mark_orphaned_by_run", "在途 run 摘除 → 看门狗计时"],
    ["pub fn sweep", "巡检（dump + 强制清理）"],
    ["record.handle.abort()", "任务级强制清理"],
    ["pub fn dumps", "dump 诊断缓冲（M3-05 消费）"],
    ["tracing::error!", "dump 按 bug 上报（M2-07 汇聚端）"],
  ];
  for (const [needle, label] of cancelRequired) {
    if (!cancel.includes(needle)) problems.push(`cancel.rs 缺少${label}: ${needle}`);
  }

  const lifecycle = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "lifecycle.rs"),
    "utf8",
  );
  const lifecycleRequired = [
    ["task_force_cleanup_ms", "强制清理阈值接线"],
    ["cancel_tree.cancel_session", "dispose 取消会话节点"],
    ["descendants_of", "父取消级联遍历"],
    ["RunCancelToken::child_of", "run 令牌为会话子节点"],
    ["mark_orphaned_by_run", "中断/超时/降级摘除计时"],
    ["task_watchdog", "看门狗接线"],
    [".sweep(", "后台看门狗巡检接线"],
    ["pub async fn session_cancel_token", "会话取消节点句柄"],
    ["pub async fn active_run_cancel_token", "在途 run 令牌（权限等待绑定）"],
    ["pub fn sweep_tasks_once", "看门狗可驱动巡检"],
    ["pub fn task_dumps", "dump 快照"],
  ];
  for (const [needle, label] of lifecycleRequired) {
    if (!lifecycle.includes(needle)) problems.push(`lifecycle.rs 缺少${label}: ${needle}`);
  }

  const permission = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "permission.rs"),
    "utf8",
  );
  for (const needle of [
    "pub async fn request_cancellable",
    "async fn try_cancel_ticket",
    "async fn finalize_cancelled",
    "permission.cancelled",
  ]) {
    if (!permission.includes(needle)) problems.push(`permission.rs 缺少 ${needle}`);
  }

  const cargoToml = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "Cargo.toml"),
    "utf8",
  );
  if (!cargoToml.includes("tokio-util")) {
    problems.push("aether-control/Cargo.toml 缺少 tokio-util（CancellationToken）");
  }

  const store = readFileSync(
    path.join(repoRoot, "crates", "aether-store", "src", "ops.rs"),
    "utf8",
  );
  if (!store.includes("pub parent_session_id: Option<String>")) {
    problems.push("aether-store/ops.rs 缺少 SessionQuery.parent_session_id");
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-05 取消树/看门狗/权限可取消/父级联查询接线在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m2-05", checks));
