#!/usr/bin/env node
/**
 * M7-05 验证：三运行时一致性回归门禁（ADR-008；RA-01/RA-02；DoD1–DoD4）。
 *
 * - DoD1：一致性测试包（三运行时 + Mock）全绿（bun 构建四个适配器 bundle →
 *   `cargo test -p aether-adapters` 全量，`AETHER_REQUIRE_*=1` 强制产物在用）；
 *   CI/夜跑接线（静态校验 `.github/workflows/ci.yml` / `m4-nightly.yml` 的 M7-05
 *   job）；真实冒烟缺凭证**显式 SKIP**（禁止静默跳过）；
 * - DoD3：P1 变更（热插拔 M7-01 / 路由 M7-02 / 暂停 M7-03 / 引用与消费映射 M7-06）
 *   后的协议面回归（含异常路径）：core 状态机/能力项、adapters 方法表、control、
 *   tauri（m7_01_runtime_control/m7_02_routing/m7_02_gate/m7_03_pause/ipc_validation）、
 *   四适配器 TS 全量；
 * - DoD2：三运行时真实冒烟回归（opt-in；沿用 M2-11 口径与变量）——
 *   `AETHER_REQUIRE_REAL_CLAUDE` + `AETHER_CLAUDE_BASE_URL/TOKEN/MODEL`；
 *   `AETHER_REQUIRE_REAL_CODEX` + `AETHER_REAL_CODEX_HOME`；
 *   `AETHER_REQUIRE_REAL_DSH` + `AETHER_REAL_DSH_BIN/HOME`；
 *   `AETHER_REAL_RUNS`（缺省 20）：各 ≥20 次 run、完成率 ≥95%、逐 run 终态、零挂起；
 * - DoD4：任一运行时失败 → ADR-008 §5 分层处置（该运行时 degraded，不阻塞其他）——
 *   由 m7_01_hotplug（隔离断言）与监督器状态机用例覆盖；门禁全绿方可进入 M8-03
 *   （§6：不得静默豁免）。
 *
 * 运行：node scripts/test/m7-05/verify-m7-05.mjs
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-05", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });
const bundleDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-05", "bundle");
mkdirSync(bundleDir, { recursive: true });

const checks = [];
const skipped = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordSkip = (name, reason) => {
  skipped.push({ name, reason });
  console.log(`[m7-05] SKIP（显式）：${name} —— ${reason}`);
};

function vitest(packageName, files) {
  return run(pnpm, [...pnpmPrefix, "--filter", packageName, "exec", "vitest", "run", ...files]);
}

const adapters = {
  mock: path.join(bundleDir, `aether-mock-adapter${exeSuffix}`),
  claude: path.join(bundleDir, `aether-claude-adapter${exeSuffix}`),
  codex: path.join(bundleDir, `aether-codex-adapter${exeSuffix}`),
  dsh: path.join(bundleDir, `aether-dsh-adapter${exeSuffix}`),
};

// ===== 静态：登记与接线 =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // ADR-008 与一致性测试包存在性。
  const adr = read("docs/adr/ADR-008-three-adapter-scope.md");
  for (const token of ["AETHER_REQUIRE_REAL_CODEX", "opt-in"]) {
    assert(adr.includes(token), `ADR-008 缺少登记项：${token}`);
  }
  for (const file of [
    "crates/aether-adapters/tests/m2_02_consistency.rs",
    "crates/aether-adapters/tests/m2_11_codex.rs",
    "crates/aether-adapters/tests/m2_11_dsh.rs",
    "crates/aether-adapters/tests/m1_09_consistency.rs",
  ]) {
    assert(existsSync(path.join(repoRoot, file)), `一致性测试文件缺失：${file}`);
  }

  // 协议面与能力项（M7-01/02/03/06 变更后仍在登记面内）。
  const protocol = read("packages/adapter-sdk/src/protocol.ts");
  for (const token of ['"session.pause": 10_000', "shutdown: 5_000", "initialize: 10_000"]) {
    assert(protocol.includes(token), `adapter-sdk protocol 缺少：${token}`);
  }
  const coreDomain = read("crates/aether-core/src/domain.rs");
  for (const token of ["THINKING_DEPTH_CAPABILITY", "PAUSE_RESUME_CAPABILITY"]) {
    assert(coreDomain.includes(token), `aether-core 能力项缺失：${token}`);
  }
  for (const file of [
    "packages/adapter-claude-code/src/claude-adapter.ts",
    "packages/adapter-codex/src/codex-adapter.ts",
    "packages/adapter-dsh/src/dsh-adapter.ts",
    "packages/adapter-mock/src/mock-adapter.ts",
  ]) {
    const text = read(file);
    assert(text.includes("pause_resume"), `${file} 缺少 pause_resume 能力声明`);
  }

  // CI/夜跑接线（DoD1）。
  const ci = read(".github/workflows/ci.yml");
  assert(ci.includes("m7-05"), "ci.yml 缺少 m7-05 一致性 job");
  const nightly = read(".github/workflows/m4-nightly.yml");
  assert(nightly.includes("m7-05"), "m4-nightly.yml 缺少 m7-05 夜跑 job");

  console.log("[static] M7-05 登记与接线：通过");
  record("静态：ADR-008/一致性测试包/协议面/能力项/CI 与夜跑接线", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record(`静态：${error.message}`, 1);
}

// ===== 构建四个适配器 bundle（一致性包 + 真实冒烟共用）=====
const bunBin =
  process.env.AETHER_BUN ?? path.join(process.env.USERPROFILE ?? "", ".bun", "bin", `bun${exeSuffix}`);
let bundleOk = true;
for (const [label, entry, outfile] of [
  ["mock", "packages/adapter-mock/src/main.ts", `aether-mock-adapter${exeSuffix}`],
  ["claude", "packages/adapter-claude-code/src/main.ts", `aether-claude-adapter${exeSuffix}`],
  ["codex", "packages/adapter-codex/src/main.ts", `aether-codex-adapter${exeSuffix}`],
  ["dsh", "packages/adapter-dsh/src/main.ts", `aether-dsh-adapter${exeSuffix}`],
]) {
  const result = run(bunBin, ["build", entry, "--compile", "--outfile", path.join(bundleDir, outfile)], {
    cwd: repoRoot,
  });
  if (result !== 0) {
    bundleOk = false;
    record(`构建 ${label} 适配器 bundle`, result);
  }
}
record("构建四适配器 bundle（Mock + 三运行时）", bundleOk ? 0 : 1);

// ===== DoD1：一致性测试包（三运行时 + Mock）全量 =====
if (bundleOk) {
  record(
    "DoD1 aether-adapters 一致性测试包全量（mock/claude/codex/dsh；REQUIRE=1 强制产物在用）",
    run(cargo, ["test", "-p", "aether-adapters", "--no-fail-fast"], {
      env: {
        ...process.env,
        AETHER_MOCK_ADAPTER: adapters.mock,
        AETHER_REQUIRE_MOCK_ADAPTER: "1",
        AETHER_CLAUDE_ADAPTER: adapters.claude,
        AETHER_REQUIRE_CLAUDE_ADAPTER: "1",
        AETHER_CODEX_ADAPTER: adapters.codex,
        AETHER_REQUIRE_CODEX_ADAPTER: "1",
        AETHER_DSH_ADAPTER: adapters.dsh,
        AETHER_REQUIRE_DSH_ADAPTER: "1",
      },
    }),
  );
} else {
  record("一致性测试包（bundle 构建失败，禁止静默跳过）", 1);
}

// ===== DoD3：P1 变更后的协议面回归（含异常路径） =====
record("aether-core --lib（状态机 22 边/能力项）", run(cargo, ["test", "-p", "aether-core", "--lib"]));
record(
  "aether-adapters --lib（D6 方法表/客户端透传：pause/references）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]),
);
record("aether-control --lib（生命周期/路由/闸门）", run(cargo, ["test", "-p", "aether-control", "--lib"]));
record(
  "DoD3 aether-tauri ipc_validation（含异常路径与 P1 增量矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);

record(
  "构建 aether-adapter-fixture",
  run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]),
);
if (!existsSync(fixture)) {
  record(`夹具产物缺失：${path.relative(repoRoot, fixture)}`, 1);
} else {
  const fixtureEnv = { ...process.env, AETHER_FIXTURE_BIN: fixture, AETHER_REQUIRE_FIXTURE: "1" };
  record(
    "DoD3 aether-tauri m7_01_runtime_control（热插拔协议面）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_01_runtime_control"], { env: fixtureEnv }),
  );
  record(
    "DoD3 aether-tauri m7_02_routing（路由协议面）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_02_routing"], { env: fixtureEnv }),
  );
  record(
    "DoD3 aether-tauri m7_02_gate（闸门/队列协议面）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_02_gate"], { env: fixtureEnv }),
  );
  record(
    "DoD3 aether-tauri m7_03_pause（暂停/恢复协议面）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_03_pause"], { env: fixtureEnv }),
  );
  record(
    "DoD4 aether-tauri m7_01_hotplug（隔离断言：单运行时故障不阻塞其他）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_01_hotplug"], { env: fixtureEnv }),
  );
}

record(
  "DoD3 adapter-sdk protocol（方法表/信封）",
  vitest("@aether/adapter-sdk", ["src/protocol.test.ts", "src/envelope.test.ts"]),
);
record(
  "DoD3 adapter-claude-code 全量",
  vitest("@aether/adapter-claude-code", [
    "src/claude-adapter.test.ts",
    "src/claude-events.test.ts",
    "src/cli.test.ts",
  ]),
);
record(
  "DoD3 adapter-codex 全量",
  vitest("@aether/adapter-codex", [
    "src/codex-adapter.test.ts",
    "src/codex-events.test.ts",
    "src/cli.test.ts",
  ]),
);
record(
  "DoD3 adapter-dsh 全量",
  vitest("@aether/adapter-dsh", [
    "src/dsh-adapter.test.ts",
    "src/dsh-delta.test.ts",
    "src/dsh-plugin.test.ts",
    "src/cli.test.ts",
  ]),
);
record(
  "DoD3 adapter-mock 全量",
  vitest("@aether/adapter-mock", ["src/mock-adapter.test.ts", "src/scenarios.test.ts", "src/cli.test.ts"]),
);

// ===== DoD2：三运行时真实冒烟（opt-in；缺凭证显式 SKIP） =====
const realRuns = process.env.AETHER_REAL_RUNS ?? "20";
// 逐 run 终态判定窗口（常量级调参，入证据）：上游网关偶发 >180s 的慢 run（实测
// 189.5s 仍完成）在 180s 窗口会被误计为「无终态」；300s 窗口下「零挂起」仍为硬断言
// （超窗即计入 hang，完成率 <95% 即失败；适配器自身 30min 兜底不变）。
const realRunTimeoutMs = process.env.AETHER_REAL_RUN_TIMEOUT_MS ?? "300000";

function optIn(name, requireEnv, credentialEnvs, command, extraEnv = {}) {
  const required = process.env[requireEnv] === "1";
  const hasCredentials = credentialEnvs.every((key) => Boolean(process.env[key]));
  if (!required && !hasCredentials) {
    recordSkip(name, `缺少 ${credentialEnvs.join("/")}（${requireEnv}=1 可强制）`);
    return;
  }
  if (required && !hasCredentials) {
    record(`${name}（${requireEnv}=1 但缺凭证，禁止静默跳过）`, 1);
    return;
  }
  record(
    name,
    run(process.execPath, command, {
      cwd: repoRoot,
      env: {
        AETHER_CLAUDE_ADAPTER: adapters.claude,
        AETHER_CODEX_ADAPTER: adapters.codex,
        AETHER_DSH_ADAPTER: adapters.dsh,
        AETHER_REAL_RUNS: realRuns,
        ...extraEnv,
      },
    }),
  );
}

optIn(
  `DoD2 Claude Code 真实冒烟（${realRuns} run；≥95%/逐 run 终态/零挂起）`,
  "AETHER_REQUIRE_REAL_CLAUDE",
  ["AETHER_CLAUDE_BASE_URL", "AETHER_CLAUDE_TOKEN"],
  [path.join(repoRoot, "scripts", "test", "m2-02", "real-claude-50.mjs")],
  {
    AETHER_REAL_CLAUDE_RUNS: realRuns,
  },
);
optIn(
  `DoD2 Codex 真实冒烟（${realRuns} run；≥95%/逐 run 终态/零挂起）`,
  "AETHER_REQUIRE_REAL_CODEX",
  ["AETHER_REAL_CODEX_HOME"],
  [
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
    "--timeout-ms",
    realRunTimeoutMs,
  ],
  process.env.OPENAI_API_KEY ? { OPENAI_API_KEY: process.env.OPENAI_API_KEY } : {},
);
optIn(
  `DoD2 DSH 真实冒烟（${realRuns} run；≥95%/逐 run 终态/零挂起）`,
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
    "--runs",
    realRuns,
    "--timeout-ms",
    realRunTimeoutMs,
  ],
);

// ===== 证据归档 =====
const summary = {
  task: "M7-05",
  stamp,
  platform: process.platform,
  evidence_dir: path.relative(repoRoot, evidenceDir),
  real_runs: Number(realRuns),
  checks,
  skipped,
};
writeFileSync(path.join(evidenceDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`, "utf8");
console.log(`[m7-05] 证据目录：${evidenceDir}`);
if (skipped.length > 0) {
  console.log(`[m7-05] 显式 SKIP ${skipped.length} 项（见 summary.json.skipped）`);
}

process.exit(summarize("verify-m7-05", checks));
