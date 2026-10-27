#!/usr/bin/env node
/**
 * M7-02 验证脚本：能力路由与并发闸门（ADR-022；DoD1–DoD5）。
 *
 * 覆盖：
 * - 静态：gate/routing 模块落点与常量（A5 公式、固定优先级常量）、DTO/bindings
 *   契约（`runtime_id` 可选、`no_candidate_runtime`）、`health.concurrency_gate`、
 *   `queue` 响应、审计动作、UI 锚点（`runtime-auto-route` / `queue-position`）与
 *   UI-UX §7.3 实现登记；
 * - DoD1/DoD2/DoD5：`m7_02_routing`（路由矩阵/热插拔联动/手动优先/模型硬条件/
 *   审计与 `routed` 回显）+ `aether-control --lib`（路由与闸门单测，含 8GB 模拟）；
 * - DoD3：`m7_02_gate`（参数注入/排队/health 快照/不超卖/FIFO 推进/A-6 合成矩阵/
 *   dispose 全清/run_retry 受限）+ 前端 `m7_02_queue_position.test.tsx`（UI 队列位置）；
 * - DoD4：`m4_04_t1_t3`（真实 Mock 适配器 T1/T2/T3——10 并发基线不退化；
 *   `--skip-t3` 或 bun 不可用时显式 SKIP，禁止静默跳过）；
 * - 回归：`m2_01_lifecycle` / `m2_05_cancel`（闸门触及的 P0 路径）、
 *   `ipc_validation` / `health_command` / `m3_02_session_backend`、`verify:m1-03`。
 *
 * 用法：node scripts/test/m7-02/verify-m7-02.mjs [--skip-t3]
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const skipT3 = process.argv.includes("--skip-t3");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-02", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordSkip = (name, reason) =>
  checks.push({ name, exit: 0, expect: 0, skip: true, reason });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

// ===== 静态检查（一次 try/catch → 单条记录） =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // 1) 闸门模块：A5 公式常量 + 采样源 + FIFO + 快照单元。
  const gate = read("crates/aether-control/src/gate.rs");
  for (const token of [
    "CONCURRENCY_GATE_MIN: usize = 2",
    "CONCURRENCY_GATE_MAX: usize = 10",
    "CONCURRENCY_GATE_RAM_PER_SLOT_GB: f64 = 1.5",
    "pub fn max_concurrent_for_free_ram",
    "pub trait FreeRamSource",
    "pub struct FixedFreeRam",
    "pub struct ConcurrencyGateCell",
    "pub struct GateQueue",
  ]) {
    assert(gate.includes(token), `gate.rs 缺少「${token}」`);
  }

  // 2) 路由模块：固定优先级常量（ADR-022 附录 A-4）+ 决策/失败类型。
  const routing = read("crates/aether-control/src/routing.rs");
  for (const token of [
    "ROUTE_PRIORITY_CLAUDE_CODE: u32 = 10",
    "ROUTE_PRIORITY_CODEX: u32 = 20",
    "ROUTE_PRIORITY_DEEPSEEK_HARNESS: u32 = 30",
    "ROUTE_PRIORITY_DEFAULT: u32 = 100",
    "pub fn route_runtime",
    "pub enum RouteFailure",
  ]) {
    assert(routing.includes(token), `routing.rs 缺少「${token}」`);
  }

  // 3) 生命周期接线：调度位、FIFO 推进、审计、路由回显。
  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  for (const token of [
    "dispatched: bool",
    "route.decided",
    "routed: options.route.as_ref().map(|_| true)",
    "fn gate_release_and_advance",
    "fn promote_waiting_locked",
    "fn refresh_gate_limit_inner",
    "pub struct QueuePosition",
  ]) {
    assert(lifecycle.includes(token), `lifecycle.rs 缺少「${token}」`);
  }

  // 4) 命令层契约：错误码 / DTO / 路由调用 / queue 响应 / health 增量。
  const error = read("crates/aether-app/src/ipc/error.rs");
  assert(error.includes("NoCandidateRuntime"), "error.rs 缺少 NoCandidateRuntime");
  assert(error.includes('Self::NoCandidateRuntime => "no_candidate_runtime"'), "错误码字符串缺失");
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  assert(dto.includes("pub runtime_id: Option<String>"), "DTO runtime_id 未改为可选");
  const backend = read("crates/aether-app/src/session_backend.rs");
  for (const token of [
    "route_runtime(",
    "no_candidate_runtime(",
    'response["queue"]',
    "provider_provides_model",
    "candidate_summary",
  ]) {
    assert(backend.includes(token), `session_backend.rs 缺少「${token}」`);
  }
  const health = read("crates/aether-app/src/core_health.rs");
  for (const token of ["concurrency_gate", "with_concurrency_gate", "ConcurrencyGateSnapshot"]) {
    assert(health.includes(token), `core_health.rs 缺少「${token}」`);
  }
  const lib = read("crates/aether-tauri/src/lib.rs");
  for (const token of ["ConcurrencyGateCell::new", "with_free_ram", "SysinfoFreeRam"]) {
    assert(lib.includes(token), `lib.rs 缺少接线「${token}」`);
  }

  // 5) 生成物（T14）：bindings 反映可选 runtime_id 与新错误码。
  const bindings = read("packages/protocol/src/bindings.ts");
  assert(bindings.includes("runtime_id?: string | null"), "bindings 缺可选 runtime_id");
  assert(bindings.includes('"no_candidate_runtime"'), "bindings 缺 no_candidate_runtime");

  // 6) UI 锚点与类型（§7.1 规则 5：新增 testid 已登记 UI-UX §7.3）。
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of [
    "runtime-auto-route",
    'data-auto={String(autoRoute)}',
    "queue-position",
    "data-position={queueNotice.position}",
    "data-limit={queueNotice.limit}",
  ]) {
    assert(workbench.includes(token), `SessionWorkbench 缺锚点「${token}」`);
  }
  const sessionTs = read("apps/desktop/src/session.ts");
  assert(sessionTs.includes("RunQueuePosition"), "session.ts 缺 RunQueuePosition");
  assert(sessionTs.includes("runtime_id?: string"), "session.ts 的 runtime_id 未改可选");
  const healthTs = read("apps/desktop/src/health.ts");
  assert(healthTs.includes("concurrency_gate"), "health.ts 缺 concurrency_gate");
  const uiux = read("docs/UI-UX-设计规格.md");
  for (const token of ["`runtime-auto-route`", "`queue-position`", "M7-02"]) {
    assert(uiux.includes(token), `UI-UX 缺锚点登记「${token}」`);
  }

  // 7) ADR 与计划登记。
  assert(
    existsSync(path.join(repoRoot, "docs/adr/ADR-022-routing-and-concurrency-gate.md")),
    "ADR-022 缺失",
  );
  const plan = read("docs/P1-实施计划与验收标准.md");
  assert(plan.includes("M7-02 能力路由与并发闸门"), "P1 计划缺 M7-02 任务节");

  record("静态：gate/routing/接线/DTO/bindings/UI 锚点/登记逐项", 0);
} catch (error) {
  record(`静态检查失败：${error && error.message ? error.message : error}`, 1);
}

// ===== DoD1（单测矩阵：路由常量/过滤/排序 + 闸门公式/队列） =====
record(
  "DoD1 aether-control 单元测试（routing/gate 含 8GB 模拟参数注入）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);

// ===== DoD1/2/5：路由命令层矩阵（固定优先级/热插拔/手动/模型/审计） =====
record(
  "DoD1/2/5 aether-tauri m7_02_routing（5 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_02_routing"]),
);

// ===== DoD3：并发闸门命令层（排队/health/不超卖/FIFO/A-6/retry） =====
record(
  "DoD3 aether-tauri m7_02_gate（5 用例：A-5/A-6 矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_02_gate"]),
);

// ===== DoD5：IPC 畸形参数矩阵回归 + health 契约 =====
record(
  "DoD5 aether-tauri ipc_validation（12 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);
record(
  "DoD5 aether-tauri health_command（5 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "health_command"]),
);
record(
  "回归 aether-tauri m3_02_session_backend（6 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_02_session_backend"]),
);

// ===== 回归：闸门触及的 P0 路径（等待队列/断流/取消风暴） =====
record(
  "回归 aether-control m2_01_lifecycle（10 用例）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"]),
);
record(
  "回归 aether-control m2_05_cancel（5 用例；20 会话 dispose 风暴按闸门口径）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_05_cancel"]),
);

// ===== DoD3（UI）：队列位置与自动路由（vitest） =====
record(
  "DoD3 前端 m7_02_queue_position（4 用例：自动路由/错误/队列位置）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m7_02_queue_position.test.tsx",
  ]),
);
record(
  "回归 前端 SessionWorkbench（5 用例）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/SessionWorkbench.test.tsx",
  ]),
);

// ===== DoD4：T3 口径 10 并发基线（真实 Mock 适配器） =====
if (skipT3) {
  recordSkip("DoD4 m4_04_t1_t3（T3 10 并发基线）", "--skip-t3 显式跳过");
} else {
  const bun = resolveBun();
  const bunExit = run(bun, ["--version"], { capture: true });
  if (bunExit !== 0) {
    recordSkip(
      "DoD4 m4_04_t1_t3（T3 10 并发基线）",
      `bun 不可用（${bun}，exit=${bunExit}）——显式跳过，禁止静默通过`,
    );
  } else {
    const mockAdapter = path.join(evidenceDir, `aether-mock-adapter${exeSuffix}`);
    const buildExit = run(
      bun,
      ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter],
      { cwd: repoRoot },
    );
    record("DoD4 构建 Mock 适配器（bun compile）", buildExit);
    if (buildExit === 0) {
      record(
        "DoD4 m4_04_t1_t3（T1/T2/T3；AETHER_REQUIRE_MOCK_ADAPTER=1）",
        run(cargo, ["test", "-p", "aether-tauri", "--test", "m4_04_t1_t3"], {
          env: {
            ...process.env,
            AETHER_MOCK_ADAPTER: mockAdapter,
            AETHER_REQUIRE_MOCK_ADAPTER: "1",
          },
        }),
      );
    }
  }
}

// ===== 回归：迁移 ↔ 附录 C ↔ 真实库双向比对（未改迁移，防回归） =====
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 证据归档 =====
const summary = {
  task: "M7-02",
  stamp,
  platform: process.platform,
  evidence_dir: path.relative(repoRoot, evidenceDir),
  skip: { t3: skipT3 },
  checks,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m7-02] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-02", checks));
