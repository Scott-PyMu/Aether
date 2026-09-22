/**
 * M2-11 / ADR-008 验证入口：三运行时适配器（Claude Code / Codex / DeepSeek Harness）。
 *
 * 覆盖：
 *   1) TS：adapter-codex / adapter-dsh 类型检查与单测（事件映射/CLI 参数/去重/权限/异常）；
 *   2) Rust：m2_11_codex（一致性 + T5a + 20 次代理）、m2_11_dsh（DoD1–8）、
 *      m2_02_consistency（Claude 回归）、m1_09_consistency（Mock 回归）；
 *   3) 静态合规：事件类型 ⊆ 附录 B、DSH 插件独立包且无 BOM、补丁 insert 行、
 *      净室检查（不 import DSH/hermes 源码、不写 DSH 包目录）；
 *   4) opt-in 真实运行时（缺凭证显式 SKIP，`AETHER_REQUIRE_REAL_*=1` 时强制）。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）；Cargo/pnpm 经 scripts/test/lib/exec.mjs 解析。
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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m2-11");
const adapters = {
  mock: path.join(tmpDir, `aether-mock-adapter${exeSuffix}`),
  claude: path.join(tmpDir, `aether-claude-adapter${exeSuffix}`),
  codex: path.join(tmpDir, `aether-codex-adapter${exeSuffix}`),
  dsh: path.join(tmpDir, `aether-dsh-adapter${exeSuffix}`),
};

// ===== 1. 编译三适配器单文件（Bun）=====

record(
  "编译 Mock 适配器（M1-09 回归）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", adapters.mock], {
    cwd: repoRoot,
  }) === 0,
);
record(
  "编译 Claude Code 适配器（M2-02 回归）",
  run(
    bun,
    ["build", "packages/adapter-claude-code/src/main.ts", "--compile", "--outfile", adapters.claude],
    { cwd: repoRoot },
  ) === 0,
);
record(
  "编译 Codex 适配器（ADR-008）",
  run(bun, ["build", "packages/adapter-codex/src/main.ts", "--compile", "--outfile", adapters.codex], {
    cwd: repoRoot,
  }) === 0,
);
record(
  "编译 DSH 适配器（M2-11）",
  run(bun, ["build", "packages/adapter-dsh/src/main.ts", "--compile", "--outfile", adapters.dsh], {
    cwd: repoRoot,
  }) === 0,
);

// ===== 2. TS：类型检查 + 单测 =====

for (const filter of [
  "@aether/adapter-codex",
  "@aether/adapter-dsh",
  "@aether/adapter-claude-code",
]) {
  record(
    `${filter} typecheck（tsc --noEmit）`,
    run(pnpm.command, [...pnpm.prefix, "--filter", filter, "typecheck"]) === 0,
  );
  record(
    `${filter} 单测（vitest）`,
    run(pnpm.command, [...pnpm.prefix, "--filter", filter, "test"]) === 0,
  );
}

// ===== 3. Rust：三适配器集成 + 回归 =====

record(
  "cargo test -p aether-adapters --lib（线协议/会话客户端/监督器/权限回环编译面）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]) === 0,
);
record(
  "m2_11_codex（一致性含异常路径 + T5a 30s Ready + Mode R 重放 + 20 次代理；原始输出见日志）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_11_codex", "--", "--nocapture"], {
    env: { AETHER_CODEX_ADAPTER: adapters.codex, AETHER_REQUIRE_CODEX_ADAPTER: "1" },
  }) === 0,
);
record(
  "m2_11_dsh（DoD1–8：门闩/注入/去重/权限回环/兜底/通道清理/20 次回归；原始输出见日志）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_11_dsh", "--", "--nocapture"], {
    env: { AETHER_DSH_ADAPTER: adapters.dsh, AETHER_REQUIRE_DSH_ADAPTER: "1" },
  }) === 0,
);
record(
  "m2_02_consistency 回归（Claude Code 一致性；ADR-008 三运行时口径）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_02_consistency"], {
    env: { AETHER_CLAUDE_ADAPTER: adapters.claude, AETHER_REQUIRE_CLAUDE_ADAPTER: "1" },
  }) === 0,
);
record(
  "m1_09_consistency 回归（Mock 5 类工具调用事件序列）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_consistency"], {
    env: { AETHER_MOCK_ADAPTER: adapters.mock, AETHER_REQUIRE_MOCK_ADAPTER: "1" },
  }) === 0,
);

// ===== 4. 静态合规（ADR-008 / A11 净室 / 附录 B）=====

{
  const problems = [];
  const appendixB = new Set([
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

  // 事件类型字面量扫描（禁止自造事件类型）。
  const eventSources = [
    path.join(repoRoot, "packages", "adapter-codex", "src", "codex-events.ts"),
    path.join(repoRoot, "packages", "adapter-codex", "src", "codex-adapter.ts"),
    path.join(repoRoot, "packages", "adapter-dsh", "src", "dsh-adapter.ts"),
  ];
  const literals = new Set();
  for (const source of eventSources) {
    const text = readFileSync(source, "utf8");
    for (const match of text.matchAll(/type:\s*"([a-z][a-z_]*\.[a-z_.]+)"/g)) {
      literals.add(match[1]);
    }
    for (const match of text.matchAll(/this\.emit\([^,]+,\s*"([a-z][a-z_]*\.[a-z_.]+)"/g)) {
      literals.add(match[1]);
    }
  }
  if (literals.size === 0) problems.push("事件类型扫描为空（检查正则/源码路径）");
  for (const literal of literals) {
    if (!appendixB.has(literal)) problems.push(`自造事件类型: ${literal}`);
  }
  console.log(`[m2-11 static] 事件类型字面量 = ${[...literals].sort().join(", ")}`);

  // DSH 插件：独立包 + 无 BOM + 不含上游 import（净室）。
  const pluginSource = readFileSync(
    path.join(repoRoot, "packages", "adapter-dsh", "src", "dsh-plugin.ts"),
    "utf8",
  );
  for (const needle of ["agent/assistant-stream", "aether-dsh-stream@1", "AETHER_DSH_DELTA_FILE"]) {
    if (!pluginSource.includes(needle)) problems.push(`dsh-plugin.ts 缺少契约标记 ${needle}`);
  }
  if (/from\s+["']@deepseek-ai\//.test(pluginSource)) {
    problems.push("dsh-plugin.ts 不得 import DSH 源码（净室合规）");
  }
  if (/hermes/i.test(pluginSource) && !/hermes 代码|hermes-studio/.test(pluginSource)) {
    // 仅允许注释中引用「不拷贝 hermes 代码」的声明。
    problems.push("dsh-plugin.ts 出现 hermes 代码引用（净室合规）");
  }
  const packageJsonLiteral = /PLUGIN_PACKAGE_JSON = `\$\{JSON\.stringify\(/.test(pluginSource);
  if (!packageJsonLiteral) {
    problems.push("插件 package.json 必须经 JSON.stringify 生成（无 BOM 保证）");
  }

  // 适配器不得 import/写入 DSH 包目录（只允许 profiles/<profile>/node_modules/<独立包名>；
  // 包名出现在升级提示文案中不属于引用）。
  const dshAdapter = readFileSync(
    path.join(repoRoot, "packages", "adapter-dsh", "src", "dsh-adapter.ts"),
    "utf8",
  );
  if (/from\s+["']@deepseek-ai\//.test(dshAdapter) || /require\(["']@deepseek-ai\//.test(dshAdapter)) {
    problems.push("dsh-adapter.ts 不得 import DSH 源码（净室合规）");
  }
  if (/join\([^)]*["']@deepseek-ai["']/.test(dshAdapter)) {
    problems.push("dsh-adapter.ts 不得写入 @deepseek-ai 包目录");
  }
  if (!dshAdapter.includes("residual_channels")) {
    problems.push("dsh-adapter.ts 缺少通道清理探针（residual_channels）");
  }
  if (!dshAdapter.includes("permissionResourceFor")) {
    problems.push("dsh-adapter.ts 缺少 M2-10 权限回环形状映射");
  }

  // Mock ④⑤ 权威夹具与权限回环分工在案。
  const sessionClient = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "session_client.rs"),
    "utf8",
  );
  if (!sessionClient.includes("ADAPTER_DISCONNECTED_CODE")) {
    problems.push("session_client.rs 缺少 ADAPTER_DISCONNECTED_CODE");
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态合规（附录 B 事件 ⊆ / 插件独立包无 BOM / 净室 / 通道探针）", problems.length === 0);
}

// ===== 5. opt-in 真实运行时（缺凭证显式 SKIP；REQUIRE=1 强制）=====

function optIn(name, requireEnv, credentialEnvs, command) {
  const required = process.env[requireEnv] === "1";
  const hasCredentials = credentialEnvs.every((key) => Boolean(process.env[key]));
  if (!required && !hasCredentials) {
    console.log(`[m2-11 opt-in] SKIP ${name}：缺少 ${credentialEnvs.join("/")}（${requireEnv}=1 可强制）`);
    return true;
  }
  if (required && !hasCredentials) {
    console.error(`[m2-11 opt-in] ${requireEnv}=1 但缺少凭证 ${credentialEnvs.join("/")}`);
    return false;
  }
  return run(process.execPath, command, {
    env: {
      AETHER_CLAUDE_ADAPTER: adapters.claude,
      AETHER_CODEX_ADAPTER: adapters.codex,
      AETHER_DSH_ADAPTER: adapters.dsh,
    },
  }) === 0;
}

const realRuns = process.env.AETHER_REAL_RUNS ?? "20";

record(
  "real-claude-50（opt-in：AETHER_REQUIRE_REAL_CLAUDE=1 或凭证齐备；≥95% 零挂起）",
  optIn("Claude Code 真实 50 次", "AETHER_REQUIRE_REAL_CLAUDE", ["AETHER_CLAUDE_BASE_URL", "AETHER_CLAUDE_TOKEN"], [
    path.join(repoRoot, "scripts", "test", "m2-02", "real-claude-50.mjs"),
  ]),
);
record(
  "codex 真实适配器 E2E（opt-in：AETHER_REAL_CODEX_HOME 或 AETHER_REQUIRE_REAL_CODEX=1；基线/Mode R/中断/N 次回归）",
  optIn("Codex 真实适配器 E2E", "AETHER_REQUIRE_REAL_CODEX", ["AETHER_REAL_CODEX_HOME"], [
    path.join(repoRoot, "scripts", "test", "m2-11", "real-adapter-e2e.mjs"),
    "--runtime",
    "codex",
    "--adapter",
    adapters.codex,
    "--codex-home",
    process.env.AETHER_REAL_CODEX_HOME ?? "",
    "--model",
    process.env.AETHER_REAL_CODEX_MODEL ?? "gpt-5.6-sol",
    "--runs",
    realRuns,
  ]),
);
record(
  "dsh 真实适配器 E2E（opt-in：AETHER_REAL_DSH_{BIN,HOME} 或 AETHER_REQUIRE_REAL_DSH=1；含插件 delta 丢失率/偏差）",
  optIn(
    "DSH 真实适配器 E2E",
    "AETHER_REQUIRE_REAL_DSH",
    ["AETHER_REAL_DSH_BIN", "AETHER_REAL_DSH_HOME"],
    [
      path.join(repoRoot, "scripts", "test", "m2-11", "real-adapter-e2e.mjs"),
      "--runtime",
      "dsh",
      "--adapter",
      adapters.dsh,
      "--dsh-bin",
      process.env.AETHER_REAL_DSH_BIN ?? "",
      "--dsh-home",
      process.env.AETHER_REAL_DSH_HOME ?? "",
      "--dsh-provider",
      process.env.AETHER_REAL_DSH_PROVIDER ?? "streamax",
      "--dsh-model",
      process.env.AETHER_REAL_DSH_MODEL ?? "deepseek-v4-pro",
      "--dsh-version",
      process.env.AETHER_REAL_DSH_VERSION ?? "0.1.5-rc.2",
      // 官方分层：llm-pi-ai composition base（同步注册，消 settings 注入竞态）。
      ...(process.env.AETHER_REAL_DSH_PROVIDER_CONFIG
        ? ["--dsh-provider-config", process.env.AETHER_REAL_DSH_PROVIDER_CONFIG]
        : []),
      "--runs",
      realRuns,
    ],
  ),
);

process.exit(summarize("verify-m2-11", checks));
