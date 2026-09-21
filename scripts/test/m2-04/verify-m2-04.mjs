/**
 * M2-04 验证入口：背压分级与 journal 补读（设计 D8、评审 #4/#9；ADR-003/ADR-004）。
 *
 * 覆盖 DoD：
 *   1) L2：写队列 >4096 → 新 run 拒绝（`storage_backpressure`），已有 run 不受影响（故障注入）；
 *   2) L3：控制投递 >会话数×5000 → 熔断 + 30s 解除；重启后从 journal 恢复投递、零丢失；
 *   3) 订阅者人为减速：心跳与中断请求响应 ≤2s（reader 不被阻塞证明）；
 *   4) `Lagged(k)` → 补读最终一致；
 *   5) 存储侧背压例外：暂停 ≤2s → 隔离 `degraded + status_reason=storage_backpressure`
 *      → 队列回落 ≤1024 持续 30s 自动解除；与 `persist_degraded` 修复路径严格区分。
 *
 * 适配器侧：M1-10 监督器 isolate/release（DoD⑤ 的 `degraded + status_reason` 真实落地）；
 * 组合层：`aether-tauri::isolation` 桥接（Windows 夹具用例，其余平台显式 SKIP）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；aether-tauri 用例仅 Windows。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");

// ===== 控制层单测：背压常量/暂停超时/隔离/解除/persist_degraded 区分 + L1 delta 放宽 =====

record(
  "cargo test -p aether-control --lib（背压控制器 + 管线：D8 常量/暂停 ≤2s/连续超时/预算熔断/解除 30s/降级冻结/L1 64ms）",
  run(cargo, ["test", "-p", "aether-control", "--lib", "--", "--nocapture"]),
);

// ===== DoD1–DoD5：集成（含故障注入；--nocapture 输出证据） =====

record(
  "m2_04_backpressure（L2 拒新 run + 已有 run 不受影响；L3 熔断/30s 解除/journal 零丢失；慢订阅者 ≤2s；Lagged 补读一致；存储例外与 persist_degraded 区分）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_04_backpressure",
    "--",
    "--nocapture",
  ]),
);

// ===== 回归：M1-05 管线/降级 + M2-01 生命周期（背压接入不得破坏既有语义） =====

record(
  "回归 aether-control 全量（m1_05_pipeline/m1_05_degraded/m1_05_attempt_log/m2_01_lifecycle/m2_03_permission）",
  run(cargo, ["test", "-p", "aether-control"]),
);

// ===== DoD⑤ 适配器侧：监督器 isolate/release（degraded + status_reason=storage_backpressure） =====

record(
  "cargo test -p aether-adapters --lib（监督状态机：storage_backpressure 词典与转移表）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]),
);
record(
  "m2_04_isolation（隔离终止进程 + degraded+storage_backpressure；解除重启新 PID；cold 不适用）",
  run(cargo, [
    "test",
    "-p",
    "aether-adapters",
    "--test",
    "m2_04_isolation",
    "--",
    "--nocapture",
  ]),
);

// ===== 组合层：IsolationSink 生产桥接（Windows 夹具；其余平台显式 SKIP） =====

if (process.platform === "win32") {
  const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
  const fixture = path.join(targetDir, "debug", "aether-adapter-fixture.exe");
  record(
    "构建适配器夹具（桥接用例输入）",
    run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]),
  );
  record(
    "aether-tauri isolation 桥接（SupervisorIsolationSink → 监督器 isolate/release）",
    run(
      cargo,
      ["test", "-p", "aether-tauri", "--test", "isolation", "--", "--nocapture"],
      {
        env: {
          AETHER_ADAPTER_FIXTURE: fixture,
          AETHER_REQUIRE_ADAPTER_FIXTURE: "1",
        },
      },
    ),
  );
} else {
  console.log(
    "SKIP（非 Windows）：aether-tauri 集成测试依赖 Windows 宿主，由 CI security-baseline（windows-2022）覆盖",
  );
}

// ===== 静态检查：D8 常量与接线在案 =====

{
  const problems = [];
  const backpressure = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "backpressure.rs"),
    "utf8",
  );
  const required = [
    ["DELIVERY_MAX_BYTES: usize = 32 * 1024 * 1024", "D8 32MB 上限"],
    ["DELIVERY_PER_SESSION_ITEMS: usize = 5_000", "D8 会话数×5000"],
    ["DELIVERY_RESTART_DELAY_MS: i64 = 30_000", "D8 30s 后重启"],
    ["DELIVERY_NO_FALL_MS: i64 = 60_000", "D8 60s 未回落"],
    ["STORAGE_L1_THRESHOLD: usize = 1_024", "D8 L1 1024"],
    ["STORAGE_L2_THRESHOLD: usize = 4_096", "D8 L2 4096"],
    ["PAUSE_WINDOW_MS: i64 = 250", "D8 暂停窗口 250ms"],
    ["PAUSE_TIMEOUT_MS: i64 = 2_000", "D8 单次暂停超时 2s"],
    ["PAUSE_BUDGET_MS: i64 = 10_000", "D8 60s 累计 >10s"],
    ["PAUSE_MAX_CONSECUTIVE_TIMEOUTS: u32 = 3", "D8 连续 3 次超时"],
    ["RELEASE_SUSTAIN_MS: i64 = 30_000", "ADR-004 回落 30s 解除"],
    ["PressurePhase::PersistDegraded", "persist_degraded 冻结路径"],
    ["fn enter_readback", "journal 补读模式切换"],
    ["fn control_read_gate", "控制读取暂停闸门"],
  ];
  for (const [needle, label] of required) {
    if (!backpressure.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  const lifecycle = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "lifecycle.rs"),
    "utf8",
  );
  for (const needle of ["fn with_backpressure", "fn check_backpressure", "AdapterIsolated"]) {
    if (!lifecycle.includes(needle)) problems.push(`lifecycle.rs 缺少 ${needle}`);
  }

  const pipeline = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "pipeline.rs"),
    "utf8",
  );
  if (!pipeline.includes("effective_delta_interval")) {
    problems.push("pipeline.rs 缺少 L1 delta 放宽接线");
  }

  const runtime = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "supervisor", "runtime.rs"),
    "utf8",
  );
  for (const needle of [
    "pub async fn isolate",
    "pub async fn release",
    "IsolationOutcome",
    "ReleaseOutcome",
  ]) {
    if (!runtime.includes(needle)) problems.push(`supervisor/runtime.rs 缺少 ${needle}`);
  }

  const bridge = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "isolation.rs"),
    "utf8",
  );
  for (const needle of [
    "impl IsolationSink for SupervisorIsolationSink",
    "DisabledReason::StorageBackpressure",
    "ReleaseOutcome::Released",
  ]) {
    if (!bridge.includes(needle)) problems.push(`aether-tauri/isolation.rs 缺少 ${needle}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-04 D8 常量/接线/隔离桥接在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m2-04", checks));
