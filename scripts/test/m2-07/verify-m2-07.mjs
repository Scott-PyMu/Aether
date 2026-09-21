/**
 * M2-07 验证入口：panic 隔离与健康巡检（设计 D2/D7、ADR-007）。
 *
 * 覆盖 DoD：
 *   1) panic 注入 → 仅该会话 failed（`task_panic`）、其余会话事件流连续（集成）；
 *   2) 核心 crate `unwrap/expect` 静态检查 0 告警（clippy + 源码扫描）；
 *   3) RSS 巡检：阈值参数化注入 → 告警事件与 delta 限流依次触发（单测 + 集成）；
 *   4) UI 15s 无响应 → 「核心未响应」+ 重启入口（组件/轮询单测；E2E 见 Windows CI）；
 *   5) UI 每 5s 轮询 `health`；E2E 覆盖 normal/persist_degraded 两态（E2E 脚本）；
 *   6) P0 运行期日志汇聚端：attempt=n/3 与 persist_degraded 可经诊断包导出（集成）。
 *
 * 用法：node scripts/test/m2-07/verify-m2-07.mjs [--skip-e2e]
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；E2E 需真实 WebView2（仅 Windows；
 *       非 Windows 显式 SKIP，由 CI security-baseline（windows-2022）执行）。
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const args = process.argv.slice(2);
const skipE2e = args.includes("--skip-e2e");

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);

// ===== DoD1：panic 隔离集成（--nocapture 输出原始断言链） =====

record(
  "m2_07_panic（panic 仅该会话 failed(task_panic)/其余会话事件流连续/提升重跑）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_07_panic",
    "--",
    "--nocapture",
  ]),
);

// ===== DoD3：RSS 巡检（单测 + 集成） =====

record(
  "cargo test -p aether-control --lib（RSS 阈值/巡检单测 + delta 窗口 L1/L2/限流口径）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);

record(
  "m2_07_resource（告警→限流依次触发/回落再告警/限流窗口放宽；--nocapture 输出证据）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_07_resource",
    "--",
    "--nocapture",
  ]),
);

// ===== DoD6：运行期日志汇聚端（capture attempt=n/3 与 persist_degraded） =====

record(
  "m2_07_logging（tracing → 环形缓冲 + 文件；attempt=1/3…3/3 + persist_degraded；--nocapture）",
  run(cargo, [
    "test",
    "-p",
    "aether-tauri",
    "--test",
    "m2_07_logging",
    "--",
    "--nocapture",
  ]),
);

// ===== DoD4/DoD5：UI 健康轮询（前端单测 + health 命令契约回归） =====

record(
  "pnpm --filter @aether/desktop test（health IPC/5s 轮询/15s 无响应/两态渲染）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

record(
  "aether-tauri health/ipc/runtime_control 回归（health 命令契约 + runtimes 映射 + core_not_ready）",
  run(cargo, [
    "test",
    "-p",
    "aether-tauri",
    "--test",
    "health_command",
    "--test",
    "ipc_validation",
    "--test",
    "runtime_control",
  ]),
);

// ===== DoD2：核心 crate unwrap/expect/panic 静态检查 0 告警 =====

record(
  "cargo clippy --workspace --all-targets -- -D warnings（unwrap/expect/panic 为 deny 级）",
  run(cargo, ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
);

{
  const problems = [];
  const crates = [
    "aether-core",
    "aether-store",
    "aether-adapters",
    "aether-control",
    "aether-security",
    "aether-tauri",
  ];
  const walk = (dir) => {
    const files = [];
    for (const entry of readdirSync(dir)) {
      const full = path.join(dir, entry);
      if (statSync(full).isDirectory()) files.push(...walk(full));
      else if (entry.endsWith(".rs")) files.push(full);
    }
    return files;
  };
  const pattern = /\.unwrap\(\)|\.expect\(|panic!\(/;
  for (const crate of crates) {
    const root = path.join(repoRoot, "crates", crate, "src");
    for (const file of walk(root)) {
      const text = readFileSync(file, "utf8");
      // 仓库约定：单测集中在文件尾部的 `#[cfg(test)]` 模块；生产段不得命中。
      const testIndex = text.indexOf("#[cfg(test)]");
      const production = testIndex >= 0 ? text.slice(0, testIndex) : text;
      production.split(/\r?\n/).forEach((line, index) => {
        const trimmed = line.trim();
        // 注释行豁免（例如「禁止 unwrap()/expect()」类文档说明）。
        if (
          trimmed.startsWith("//") ||
          trimmed.startsWith("*") ||
          trimmed.startsWith("/*")
        ) {
          return;
        }
        if (pattern.test(line)) {
          problems.push(
            `${path.relative(repoRoot, file)}:${index + 1}: ${line.trim()}`,
          );
        }
      });
    }
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "核心 crate unwrap/expect/panic 静态扫描 0 命中（排除 #[cfg(test)] 段）",
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 静态检查：M2-07 关键接线在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  for (const [needle, label] of [
    ["JoinSet<()>", "D2：会话任务经 JoinSet 管理"],
    ["try_join_next_with_id", "JoinError 收割入口"],
    ["error.is_panic()", "panic 判定"],
    ['RUN_TASK_PANIC_CODE: &str = "task_panic"', "panic 终态错误码"],
    ["reap_run_tasks", "panic 收割与恢复"],
    ["restart_session", "panic 后 sequencer 恢复（D4）"],
  ]) {
    if (!lifecycle.includes(needle)) problems.push(`lifecycle.rs 缺少${label}: ${needle}`);
  }

  const pipeline = read("crates/aether-control/src/pipeline.rs");
  for (const [needle, label] of [
    ["RSS_ALERT_BYTES: u64 = 2 * 1024 * 1024 * 1024", "D2：2GB 告警"],
    ["RSS_THROTTLE_BYTES: u64 = 5 * 1024 * 1024 * 1024 / 2", "D2：2.5GB 限流"],
    ["RSS_THROTTLE_DELTA_INTERVAL: Duration = Duration::from_millis(256)", "限流窗口"],
    ["pub async fn report_resource_pressure", "巡检上报入口"],
    ["RSS_ALERT_EVENT_CODE", "告警事件码"],
    ["RSS_THROTTLE_EVENT_CODE", "限流事件码"],
  ]) {
    if (!pipeline.includes(needle)) problems.push(`pipeline.rs 缺少${label}: ${needle}`);
  }

  const patrol = read("crates/aether-control/src/resource_patrol.rs");
  for (const [needle, label] of [
    ["RESOURCE_PATROL_INTERVAL: Duration = Duration::from_secs(5)", "5s 采样"],
    ['ENV_RSS_ALERT_MB: &str = "AETHER_TEST_RSS_ALERT_MB"', "阈值注入（告警）"],
    ['ENV_RSS_THROTTLE_MB: &str = "AETHER_TEST_RSS_THROTTLE_MB"', "阈值注入（限流）"],
    ["SysinfoRssSampler", "sysinfo 采样实现"],
  ]) {
    if (!patrol.includes(needle)) problems.push(`resource_patrol.rs 缺少${label}: ${needle}`);
  }

  const coreHealth = read("crates/aether-tauri/src/core_health.rs");
  for (const [needle, label] of [
    ["SupervisorRuntimeSummaries", "监督器摘要源（ADR-007 §5-2）"],
    ["boot_core_health_with", "摘要源注入启动"],
    ["summary_snapshot", "监督器状态映射"],
  ]) {
    if (!coreHealth.includes(needle)) problems.push(`core_health.rs 缺少${label}: ${needle}`);
  }

  const logging = read("crates/aether-tauri/src/logging.rs");
  for (const [needle, label] of [
    ['LOG_FILE_NAME: &str = "aether.log"', "日志文件名"],
    ["pub fn init_for_data_dir", "生产接线入口"],
    ["pub fn export_text", "诊断包导出源"],
    ["MakeWriter", "tracing 订阅端"],
  ]) {
    if (!logging.includes(needle)) problems.push(`logging.rs 缺少${label}: ${needle}`);
  }

  const healthTs = read("apps/desktop/src/health.ts");
  for (const [needle, label] of [
    ["HEALTH_POLL_INTERVAL_MS = 5_000", "UI 5s 轮询常量"],
    ["HEALTH_TIMEOUT_MS = 15_000", "UI 15s 超时常量"],
    ['invoke<HealthReport>("health")', "health 命令调用"],
  ]) {
    if (!healthTs.includes(needle)) problems.push(`health.ts 缺少${label}: ${needle}`);
  }

  const monitor = read("apps/desktop/src/HealthMonitor.tsx");
  for (const [needle, label] of [
    ['data-testid="core-unresponsive"', "「核心未响应」锚点"],
    ['data-testid="core-restart"', "重启入口锚点"],
    ['data-testid="storage-degraded"', "降级横幅锚点"],
    ['data-testid="health-normal"', "正常态锚点（E2E）"],
  ]) {
    if (!monitor.includes(needle)) problems.push(`HealthMonitor.tsx 缺少${label}: ${needle}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-07 panic 隔离/RSS 巡检/health runtimes/日志汇聚/UI 锚点接线在案", problems.length === 0 ? 0 : 1);
}

// ===== DoD4/DoD5 E2E（真实 WebView2；仅 Windows，非 Windows 显式 SKIP） =====

if (skipE2e) {
  console.log("SKIP（--skip-e2e）：health 监控 E2E 由 Windows CI security-baseline 执行");
} else if (process.platform === "win32") {
  record(
    "m2-07-health-monitor-e2e（normal/persist_degraded 两态 + 15s 无响应 + 日志产物）",
    run(process.execPath, [
      path.join(repoRoot, "scripts", "test", "m2-07", "e2e-health-monitor.mjs"),
    ]),
  );
} else {
  console.log(
    "SKIP（非 Windows）：health 监控 E2E 依赖 WebView2 宿主，由 CI security-baseline（windows-2022）覆盖",
  );
}

process.exit(summarize("verify-m2-07", checks));
