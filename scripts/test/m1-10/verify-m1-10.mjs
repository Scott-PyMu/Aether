/**
 * M1-10 验证脚本：适配器监督器（D5、评审#3/#4/#5）。
 *
 * 覆盖 DoD：
 *   1) 状态机与 runtimes.status/status_reason 一一对应 + 每次转移广播
 *      runtime.status_changed（单测 `supervisor::state` + 集成 `m1_10_supervisor`）；
 *   2) 退避 1/2/4/8/16/30s 与 60s ≥5 次熔断 crash_loop（单测 `supervisor::backoff`
 *      + 集成熔断→enable 恢复）；
 *   3) 进程组/Job Object：setsid（Unix pgrp 断言）/CREATE_NEW_PROCESS_GROUP + Job Object
 *      （Windows 终止机制断言）；终止序列逐步硬超时（RPC 5s → TERM/taskkill /T 5s →
 *      KILL/TerminateJobObject → taskkill /T /F 兜底，集成真实进程树 + 平台断言）；
 *   4) PID 台账三条件：存活 + 启动时间 + launch_token；复用夹具不误杀（集成）；
 *   5) 非官方 manifest → disabled + untrusted + 审计（集成）；
 *   6) 启动即崩：stderr 尾 50 行 + start_failed（集成）；
 *   7) runtime_retry / runtime_enable 参数与状态校验 + disabled → cold → starting（集成）。
 *
 * 回归：M1-09 一致性/健壮性测试（进程宿主重构后必须保持全绿）。
 *
 * 顺序说明：夹具进程会在测试结束后短暂占用夹具可执行文件（Windows 文件锁），
 * 因此先跑 M1-09 回归、最后跑 M1-10 夹具测试，避免重链接竞争。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；Bun 用于编译 Mock（回归用）。
 */
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, ok) => checks.push({ name, exit: ok ? 0 : 1, expect: 0 });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const bun = resolveBun();

// ===== 0. 预构建全部测试目标（避免 Windows 上夹具进程退出与重链接的文件锁竞争） =====

record(
  "cargo test -p aether-adapters --no-run（预构建测试目标与夹具 bin）",
  run(cargo, ["test", "-p", "aether-adapters", "--no-run"]) === 0,
);

// ===== 1. 监督器单测（纯逻辑：状态机/退避/心跳/准入/台账/资源/终止序列） =====

record(
  "cargo test -p aether-adapters --lib（状态机/退避曲线/心跳/准入/台账三条件/终止序列硬超时）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]) === 0,
);

// ===== 2. 回归：M1-09 线协议测试（进程宿主重构后全绿；夹具测试之前执行） =====

const mockBinary = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m1-09",
  process.platform === "win32" ? "aether-mock-adapter.exe" : "aether-mock-adapter",
);
record(
  "编译 Mock 适配器单文件（bun build --compile，回归用）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockBinary], {
    cwd: repoRoot,
  }) === 0,
);

const mockEnv = {
  AETHER_MOCK_ADAPTER: mockBinary,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
};
record(
  "回归 m1_09_consistency（真实 Mock：握手/流式/中断/dispose/shutdown）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_consistency"], {
    env: mockEnv,
  }) === 0,
);
record(
  "回归 m1_09_robustness（真实 Mock：健壮性注入/版本不匹配/2MiB 契约）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_robustness"], {
    env: mockEnv,
  }) === 0,
);

// ===== 3. 集成：PID 台账三条件（真实进程夹具） =====

record(
  "m1_10_ledger（三条件全命中回收；时间/token 不符不杀；陈旧记录清理；监督器启动清理审计）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_10_ledger"]) === 0,
);

// ===== 4. 集成：终止序列与平台断言（--nocapture 输出机制序列作为证据） =====

record(
  "m1_10_termination（整树回收/逐步硬超时/平台机制断言，机制序列见输出）",
  run(cargo, [
    "test",
    "-p",
    "aether-adapters",
    "--test",
    "m1_10_termination",
    "--",
    "--nocapture",
  ]) === 0,
);

// ===== 5. 集成：监督器全流程（预热/心跳/崩溃恢复/启动即崩/准入/熔断/命令语义） =====

record(
  "m1_10_supervisor（预热 ready、心跳 ×3 → 重启、崩溃恢复、stderr 尾 50、untrusted、crash_loop、retry/enable）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_10_supervisor"]) === 0,
);

process.exit(summarize("verify-m1-10", checks));
