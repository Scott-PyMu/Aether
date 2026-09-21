/**
 * M2-02 验证入口：首个真实适配器（Claude Code；A1 real-adapter 路径）。
 *
 * 覆盖 DoD：
 *   1) 一致性测试全用例（含异常路径）：真实适配器进程 + fake-claude 夹具 →
 *      `m2_02_consistency`（握手/流式/中断/dispose/tools.list/permission 边界/Mode R）；
 *   2) 工具定义与调用事件（附录 B）：`tools.list` + `tool.call_started/completed/failed`；
 *      Mock 侧 5 类工具调用注入清单（M1-09 权威夹具）回归单测 + 集成；
 *   3) 崩溃注入 T5a：外部强杀 → 30s 内 Ready + 在途 run 标 failed（Disconnected）
 *      + Mode R 重放重试 → `m2_02_t5a`；
 *   4) 连续 50 次完成率 ≥95%（真实 Runtime，opt-in；缺凭证显式 SKIP，禁止静默）：
 *      `real-claude-50.mjs`（`AETHER_REQUIRE_REAL_CLAUDE=1` 时强制）。
 *
 * 环境：
 *   - Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译适配器单文件；
 *   - Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；
 *   - 真实 Runtime 步骤需要 `AETHER_CLAUDE_BASE_URL` / `AETHER_CLAUDE_TOKEN`。
 */
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

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
const pnpm = pnpmCommand();
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m2-02");
const claudeAdapter = path.join(tmpDir, `aether-claude-adapter${exeSuffix}`);
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);

// ===== 1. 编译适配器单文件（Bun） =====

record(
  "编译 Claude Code 适配器单文件（bun build --compile）",
  run(
    bun,
    ["build", "packages/adapter-claude-code/src/main.ts", "--compile", "--outfile", claudeAdapter],
    { cwd: repoRoot },
  ) === 0,
);
record(
  "编译 Mock 适配器单文件（DoD2 5 类工具调用回归）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }) === 0,
);

// ===== 2. TS：类型检查 + 单测（事件映射/异常路径/D6 方法） =====

record(
  "adapter-claude-code typecheck（tsc --noEmit）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-claude-code", "typecheck"]) === 0,
);
record(
  "adapter-claude-code 单测（事件映射/工具序列/异常路径/CLI 参数/Mode R）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-claude-code", "test"]) === 0,
);
record(
  "adapter-mock 单测（5 类工具调用 + DoD6 权威夹具一致性；M1-09 回归）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-mock", "test"]) === 0,
);

// ===== 3. Rust：会话客户端单测 + M2-02 集成（DoD1/DoD2/DoD3） =====

record(
  "cargo test -p aether-adapters --lib（线协议/连接/监督器/会话客户端编译面）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]) === 0,
);

const claudeEnv = {
  AETHER_CLAUDE_ADAPTER: claudeAdapter,
  AETHER_REQUIRE_CLAUDE_ADAPTER: "1",
};

record(
  "m2_02_consistency（一致性全用例含异常路径；原始输出见日志）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_02_consistency", "--", "--nocapture"], {
    env: claudeEnv,
  }) === 0,
);
record(
  "m2_02_t5a（外部强杀 → 30s 内 Ready + 在途 run 收口 + Mode R 重放；20 次完成率代理；原始输出见日志）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_02_t5a", "--", "--nocapture"], {
    env: claudeEnv,
  }) === 0,
);
record(
  "m1_09_consistency 回归（Mock 5 类工具调用事件序列，DoD2）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_consistency"], {
    env: { AETHER_MOCK_ADAPTER: mockAdapter, AETHER_REQUIRE_MOCK_ADAPTER: "1" },
  }) === 0,
);

// ===== 4. 静态检查：自造事件类型防护 + 接线在案 =====

{
  const problems = [];
  const eventsSource = readFileSync(
    path.join(repoRoot, "packages", "adapter-claude-code", "src", "claude-events.ts"),
    "utf8",
  );
  const adapterSource = readFileSync(
    path.join(repoRoot, "packages", "adapter-claude-code", "src", "claude-adapter.ts"),
    "utf8",
  );
  // 附录 B MVP 集（AGENTS §2.3：不得自造事件类型）。
  const allowed = new Set([
    "session.created",
    "session.updated",
    "session.status_changed",
    "session.closed",
    "run.started",
    "run.completed",
    "run.failed",
    "run.cancelled",
    "message.delta",
    "message.completed",
    "tool.call_started",
    "tool.call_completed",
    "tool.call_failed",
    "permission.requested",
    "permission.resolved",
    "runtime.status_changed",
    "usage",
    "log",
    "error",
  ]);
  const literals = new Set();
  for (const source of [eventsSource, adapterSource]) {
    for (const match of source.matchAll(/type:\s*"([a-z][a-z_]*\.[a-z_.]+)"/g)) {
      literals.add(match[1]);
    }
    for (const match of source.matchAll(/this\.emit\([^,]+,\s*"([a-z][a-z_]*\.[a-z_.]+)"/g)) {
      literals.add(match[1]);
    }
  }
  if (literals.size === 0) problems.push("事件类型扫描为空（检查正则/源码路径）");
  for (const literal of literals) {
    if (!allowed.has(literal)) problems.push(`自造事件类型: ${literal}（不在附录 B MVP 集）`);
  }
  console.log(`[m2-02 static] 适配器事件类型字面量 = ${[...literals].sort().join(", ")}`);

  const clientSource = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "session_client.rs"),
    "utf8",
  );
  for (const needle of ["AdapterSessionClient", "ADAPTER_DISCONNECTED_CODE", "wait_run_outcome"]) {
    if (!clientSource.includes(needle)) problems.push(`session_client.rs 缺少 ${needle}`);
  }
  const runtimeSource = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "supervisor", "runtime.rs"),
    "utf8",
  );
  if (!runtimeSource.includes("pub async fn connection(")) {
    problems.push("supervisor/runtime.rs 缺少 connection() 访问器（M2-02）");
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：事件类型 ⊆ 附录 B；会话客户端/监督器接线在案", problems.length === 0);
}

// ===== 5. DoD4：真实 Runtime 50 次完成率（opt-in；缺凭证显式 SKIP/强制失败） =====

record(
  "real-claude-50（opt-in：AETHER_REQUIRE_REAL_CLAUDE=1 或凭证齐备时执行；≥95% 且零挂起）",
  run(process.execPath, [path.join(repoRoot, "scripts", "test", "m2-02", "real-claude-50.mjs")], {
    env: { AETHER_CLAUDE_ADAPTER: claudeAdapter },
  }) === 0,
);

process.exit(summarize("verify-m2-02", checks));
