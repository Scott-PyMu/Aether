/**
 * M2-01 验证入口：生命周期管理与 run 串行（主链起点；设计 D2/D5/D8、ADR-005）。
 *
 * 覆盖 DoD：
 *   1) 状态机全转移单测（aether-core `session_state`）+ 集成终态拒绝；
 *   2) 并行发送 3 条 → 1 执行 / 1 排队 / 第 3 条 `session_busy`（集成）；
 *   3) 同一 `client_msg_id` 重发不重复；**重启核心后重放同值仍不重复**（集成）；
 *   4) ack 快路径：消息与 run 行提交后立即 ack（时序断言，集成）；
 *   5) 120s 无事件 → run failed 且可重试（时钟注入，集成）；
 *   6) `runtime_retry` / `runtime_enable` 接入 `IpcBackend`（aether-tauri 集成 +
 *      M1-08 校验矩阵回归）。
 *
 * 回归互链：ADR-007 写失败重试 attempt=1/3…3/3（M1-05 既有用例，Gate 2 检查项 #6）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；aether-tauri 用例仅 Windows
 * （WebView2 宿主；Linux/macOS 由 CI 对应 job 覆盖，本机非 Windows 显式 SKIP）。
 */
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");

// ===== DoD1：状态机单测（全转移 + 非法拒绝 + 终态无出边） =====

record(
  "cargo test -p aether-core --lib（会话状态机全转移/非法拒绝/终态无出边）",
  run(cargo, ["test", "-p", "aether-core", "--lib"]),
);

// ===== 存储层：幂等单事务（BeginRunIdempotent）+ 领域写命令单测 =====

record(
  "cargo test -p aether-store --lib（领域写命令/幂等去重/消息 seq 自动分配）",
  run(cargo, ["test", "-p", "aether-store", "--lib"]),
);

// ===== 控制层单测（配置基线/错误码/超时错误可重试） =====

record(
  "cargo test -p aether-control --lib（生命周期配置基线/错误码/可重试判定）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);

// ===== DoD2–DoD5：集成（串行/幂等重启重放/ack 时序/120s 断流；--nocapture 输出证据） =====

record(
  "m2_01_lifecycle（1 执行/1 排队/session_busy、重启重放、ack 时序、120s 断流，原始输出见日志）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_01_lifecycle",
    "--",
    "--nocapture",
  ]),
);

// ===== 回归互链：ADR-007 写失败重试 attempt=n/3（Gate 2 检查项 #6） =====

record(
  "回归 m1_05_attempt_log（MAX_WRITE_ATTEMPTS=3 含首次，attempt=1/3…3/3）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m1_05_attempt_log"]),
);

// ===== DoD6：IPC 接线（Windows 宿主；非 Windows 显式 SKIP 说明） =====

if (process.platform === "win32") {
  record(
    "m2-01 IPC 接线（runtime_retry/enable 状态转移 + M1-08 校验矩阵回归；原始输出见日志）",
    run(cargo, [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "runtime_control",
      "--",
      "--nocapture",
    ]),
  );
} else {
  console.log(
    "SKIP（非 Windows）：aether-tauri IPC 接线用例依赖 WebView2 宿主，由 CI security-baseline（windows-2022）覆盖",
  );
}

// ===== 静态检查：关键常量与接线在案 =====

{
  const { readFileSync } = await import("node:fs");
  const lifecycle = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "lifecycle.rs"),
    "utf8",
  );
  const problems = [];
  const required = [
    ["RUN_STREAM_TIMEOUT_MS: i64 = 120_000", "120s 断流常量"],
    ["MAX_WAITING_RUNS_PER_SESSION: usize = 1", "等待队列 1"],
    ["StoreCommand::BeginRunIdempotent", "持久化幂等单事务"],
    ["RUN_STREAM_TIMEOUT_CODE: &str = \"run_stream_timeout\"", "断流错误码"],
  ];
  for (const [needle, label] of required) {
    if (!lifecycle.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  const tauriCargo = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "Cargo.toml"),
    "utf8",
  );
  if (!tauriCargo.includes("aether-adapters.workspace = true")) {
    problems.push("aether-tauri 未接线 aether-adapters（runtime_* 薄适配）");
  }
  const backend = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "runtime_control.rs"),
    "utf8",
  );
  for (const needle of [
    "fn runtime_retry",
    "fn runtime_enable",
    "map_supervisor_error",
    "RuntimeControlBackend",
  ]) {
    if (!backend.includes(needle)) problems.push(`runtime_control.rs 缺少 ${needle}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-01 常量/接线在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m2-01", checks));
