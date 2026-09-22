/**
 * M2-10 验证入口：权限回环集成测试（设计 D9/D6、评审 #1、ADR-004）。
 *
 * 覆盖 DoD（实施计划 v1.13 §3 / M2 实施计划 §5）：
 *   1) 基础回环（必须通过）：适配器文件类工具调用 100% 经 `permission.request` 回环，
 *      零直通（探针计数断言：收到 = 决议 = 下发）——`m2_10_permission_loop` 集成用例；
 *   2) 异常路径：deny / 超时 deny / once / session 授权 / 重启后 pending 决议
 *      ——行为与审计断言（同上集成用例）；
 *   3) 回环事件序列证据可导出归档（`AETHER_M2_10_EVIDENCE_DIR` 下逐用例 JSON）。
 *
 * 环境：
 *   - Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *   - Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；
 *   - 真实 Mock 进程由 `AETHER_MOCK_ADAPTER` 注入（本脚本构建并强制要求）。
 */
import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, result, expect = 0) =>
  checks.push({
    name,
    // 兼容两种调用：布尔判定（true=通过）与 `run()` 退出码。
    exit: typeof result === "boolean" ? (result ? 0 : 1) : result,
    expect,
  });

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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m2-10");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

// ===== 1. 编译 Mock 适配器单文件（Bun；真实回环宿主）=====

record(
  "编译 Mock 适配器单文件（bun build --compile；M2-10 真实回环宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }) === 0,
);

// ===== 2. TS：Mock 回环场景单测 + 类型检查 =====

record(
  "adapter-mock typecheck（tsc --noEmit）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-mock", "typecheck"]) === 0,
);
record(
  "adapter-mock 单测（真实回环：发 permission.request / 等 permission.resolve / allow·deny·中断收口）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-mock", "test"]) === 0,
);

// ===== 3. Rust：回环单元测试 + 端到端集成测试（真实 Mock + 真实网关）=====

record(
  "cargo test -p aether-adapters --lib permission_loop（解析/决议下发参数/探针零直通判定）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib", "permission_loop"]) === 0,
);

mkdirSync(evidenceDir, { recursive: true });
const integrationEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_M2_10_EVIDENCE_DIR: evidenceDir,
};
record(
  "m2_10_permission_loop（DoD1 基础回环零直通 + DoD2 deny/超时/once/session/重启 pending；原始输出见日志）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m2_10_permission_loop", "--", "--nocapture"],
    { env: integrationEnv },
  ) === 0,
);

// ===== 4. 静态检查（回环契约与边界声明在案）=====

{
  const problems = [];
  const loopSource = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "permission_loop.rs"),
    "utf8",
  );
  const gateSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "permission_loop.rs"),
    "utf8",
  );
  const testSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "tests", "m2_10_permission_loop.rs"),
    "utf8",
  );
  const mockSource = readFileSync(
    path.join(repoRoot, "packages", "adapter-mock", "src", "mock-adapter.ts"),
    "utf8",
  );
  const required = [
    [loopSource, "仅约束适配器经线协议上报的工具调用", "D9 边界口径（adapters 回环模块文档）"],
    [gateSource, "仅约束适配器经线协议上报的工具调用", "D9 边界口径（tauri 网关接线文档）"],
    [loopSource, "zero_passthrough", "零直通判定（探针）"],
    [loopSource, "PERMISSION_RESOLVE_TIMEOUT", "permission.resolve 5s 超时（D6）"],
    [testSource, "zero_passthrough", "集成测试零直通断言"],
    [testSource, "permission.requested", "核心事件序列断言"],
    [testSource, "permission.timeout", "超时 deny 审计断言"],
    [testSource, "permission.session_grant_hit", "session 授权审计断言"],
    [testSource, "restore_pending", "重启后 pending 决议断言"],
    [mockSource, "permissionWaiters", "Mock 等待核心决议（不预设）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：回环契约/边界声明/证据断言在案", problems.length === 0 ? 0 : 1);
}

// ===== 5. 证据归档（DoD3）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  let zeroPassthroughAll = files.length > 0;
  const lines = [];
  for (const name of files.sort()) {
    const value = JSON.parse(readFileSync(path.join(evidenceDir, name), "utf8"));
    if (value.probe) {
      lines.push(
        `${name}: ${value.task ?? ""} → requests=${value.probe.requests_received} decisions=${value.probe.decisions} resolves=${value.probe.resolutions_sent} zero_passthrough=${value.probe.zero_passthrough}`,
      );
      if (!value.probe.zero_passthrough) zeroPassthroughAll = false;
    } else {
      lines.push(`${name}: ${value.task ?? ""}`);
    }
  }
  console.log("[m2-10] 证据文件：");
  for (const line of lines) console.log(`  - ${line}`);
  console.log(`[m2-10] 证据目录：${evidenceDir}`);
  record(
    `证据归档（DoD3）：${files.length} 份逐用例证据 + 全部探针零直通`,
    files.length >= 5 && zeroPassthroughAll ? 0 : 1,
  );
}

process.exit(summarize("verify-m2-10", checks));
