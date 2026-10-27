#!/usr/bin/env node
/**
 * M7-03 验证脚本：会话暂停/恢复（ADR-017；DoD1–DoD6）。
 *
 * 覆盖：
 * - 静态：22 边状态机与 `PAUSE_RESUME_CAPABILITY`、生命周期 pause/resume/标记/门禁/
 *   审计/诊断统计、D6 `session.pause`（10s；adapters + adapter-sdk）、四适配器
 *   `pause_resume` 声明与处理、Mock `--no-pause-resume` 负向夹具、`pause_unsupported`
 *   错误码、命令注册三处与 bindings、UI 锚点（`session-pause-toggle`）/文案/UI-UX 登记；
 * - DoD1/2/4/5/6：`m7_03_pause`（aether-control 10 用例：状态机/交互矩阵/重启/审计/统计）
 *   + `aether-core --lib`（22 边白名单）；
 * - DoD3：夹具构建 + `m7_03_pause`（aether-tauri 3 用例：能力门/端到端/非法矩阵）
 *   + 四适配器 TS 用例（能力声明 + `session.pause` 处理）；
 * - DoD6：诊断 `session_pause` 段（`pause_stats` 单测 + 源静态检查）；
 * - 回归：`ipc_validation`（新增 pause/resume 畸形矩阵）、`m2_01_lifecycle`、
 *   `m2_05_cancel`、`m3_06_retry`、`m3_12_waiting_state`、`m3_02_session_backend`、
 *   前端 `SessionWorkbench`、`verify:m1-03`（迁移未改）。
 *
 * 用法：node scripts/test/m7-03/verify-m7-03.mjs
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
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-03", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function vitest(packageName, files) {
  return run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    packageName,
    "exec",
    "vitest",
    "run",
    ...files,
  ]);
}

// ===== 静态检查（一次 try/catch → 单条记录） =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // 1) ADR-017 与设计登记。
  assert(
    existsSync(path.join(repoRoot, "docs/adr/ADR-017-pause-resume-semantics.md")),
    "ADR-017 缺失",
  );
  const adr = read("docs/adr/ADR-017-pause-resume-semantics.md");
  for (const token of ["session.pause", "pause_resume", "pause_unsupported", "user_pause"]) {
    assert(adr.includes(token), `ADR-017 缺少「${token}」`);
  }

  // 2) 核心：22 边状态机 + 能力项常量。
  const core = read("crates/aether-core/src/session_state.rs");
  for (const token of [
    "legal.len(),",
    "22,",
    "(Idle, Paused)",
    "(Paused, Idle)",
    "(WaitingPermission, Paused)",
  ]) {
    assert(core.includes(token), `session_state.rs 缺少「${token}」`);
  }
  const domain = read("crates/aether-core/src/domain.rs");
  assert(
    domain.includes('PAUSE_RESUME_CAPABILITY: &str = "pause_resume"'),
    "domain.rs 缺少 PAUSE_RESUME_CAPABILITY",
  );

  // 3) 生命周期：pause/resume/标记/门禁/审计/统计/重启语义。
  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  for (const token of [
    "pub async fn pause(",
    "pub async fn resume(",
    "pub trait PauseNotifier",
    "pub struct PauseMarker",
    "pub struct PauseReport",
    "pub struct ResumeReport",
    "pub struct PauseStats",
    "pub async fn pause_stats(",
    '"user_pause"',
    "SessionPaused {",
    '"session_paused"',
    "pause_marker",
    "pause_notify",
    '"session.pause"',
    '"session.resume"',
  ]) {
    assert(lifecycle.includes(token), `lifecycle.rs 缺少「${token}」`);
  }
  assert(
    lifecycle.includes("重启收口：paused 会话保留") &&
      lifecycle
        .slice(lifecycle.indexOf("重启收口：paused 会话保留"), lifecycle.indexOf("重启收口：paused 会话保留") + 400)
        .includes("continue;"),
    "reconcile_interrupted_runs 必须保留 paused（不收口；ADR-017 §2.1-6）",
  );

  // 4) D6 方法表：adapters + adapter-sdk（10s）。
  const protocol = read("crates/aether-adapters/src/protocol.rs");
  for (const token of ["SessionPause", '"session.pause"', "ALL: [Self; 10]"]) {
    assert(protocol.includes(token), `adapters/protocol.rs 缺少「${token}」`);
  }
  const sessionClient = read("crates/aether-adapters/src/session_client.rs");
  assert(sessionClient.includes("pub async fn pause("), "session_client.rs 缺少 pause");
  const sdkProtocol = read("packages/adapter-sdk/src/protocol.ts");
  assert(sdkProtocol.includes('"session.pause": 10_000'), "adapter-sdk 缺少 session.pause 超时");

  // 5) 四适配器能力声明与处理 + Mock 负向夹具。
  for (const [file, label] of [
    ["packages/adapter-claude-code/src/claude-adapter.ts", "claude"],
    ["packages/adapter-codex/src/codex-adapter.ts", "codex"],
    ["packages/adapter-dsh/src/dsh-adapter.ts", "dsh"],
    ["packages/adapter-mock/src/mock-adapter.ts", "mock"],
  ]) {
    const source = read(file);
    assert(source.includes('"pause_resume"'), `${label} 适配器缺少 pause_resume 能力`);
    assert(
      source.includes('"session.pause"') || source.includes("session.pause"),
      `${label} 适配器缺少 session.pause 处理`,
    );
  }
  assert(
    read("packages/adapter-mock/src/cli.ts").includes("--no-pause-resume"),
    "Mock CLI 缺少 --no-pause-resume 负向夹具",
  );
  const fixtureSource = read("crates/aether-adapters/src/bin/aether-adapter-fixture.rs");
  for (const token of ['"pause_resume"', "session.pause", "--no-pause-resume", "long-once"]) {
    assert(fixtureSource.includes(token), `夹具缺少「${token}」`);
  }

  // 6) 命令层：错误码 / 命令注册三处 / bindings / 能力门与业务码。
  const errorSource = read("crates/aether-app/src/ipc/error.rs");
  assert(errorSource.includes("PauseUnsupported"), "error.rs 缺少 PauseUnsupported");
  assert(
    errorSource.includes('Self::PauseUnsupported => "pause_unsupported"'),
    "pause_unsupported 字符串缺失",
  );
  const commandsSource = read("crates/aether-tauri/src/ipc/commands.rs");
  for (const command of ["session_pause", "session_resume"]) {
    const mentions = (commandsSource.match(new RegExp(command, "g")) ?? []).length;
    assert(mentions >= 4, `命令 ${command} 注册点不足：${mentions}`);
  }
  const backendSource = read("crates/aether-app/src/session_backend.rs");
  for (const token of [
    "fn session_pause(",
    "fn session_resume(",
    "ensure_pause_supported",
    "ensure_runtime_available",
    "pause_unsupported(",
    "session_not_found",
  ]) {
    assert(backendSource.includes(token), `session_backend.rs 缺少「${token}」`);
  }
  const executorSource = read("crates/aether-app/src/adapter_executor.rs");
  for (const token of ["pause_session", "impl aether_control::PauseNotifier for AdapterRunExecutor"]) {
    assert(executorSource.includes(token), `adapter_executor.rs 缺少「${token}」`);
  }
  const libSource = read("crates/aether-tauri/src/lib.rs");
  assert(libSource.includes("with_pause_notifier"), "lib.rs 缺少 pause notifier 接线");
  assert(libSource.includes("session_manager: session_manager.clone()"), "lib.rs 缺少诊断接线");
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  assert(diagnostics.includes('"session_pause"'), "诊断包缺少 session_pause 段");
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const token of ["sessionPause", "sessionResume", '"pause_unsupported"']) {
    assert(bindings.includes(token), `bindings.ts 缺少「${token}」`);
  }

  // 7) UI：锚点/文案/门禁。
  const sessionTs = read("apps/desktop/src/session.ts");
  for (const token of [
    "PAUSE_RESUME_CAPABILITY",
    "PAUSE_UNSUPPORTED_HINT",
    "PAUSE_SEMANTICS_NOTICE",
    "PAUSED_STATUS_HINT",
    "pauseSession",
    "resumeSession",
  ]) {
    assert(sessionTs.includes(token), `session.ts 缺少「${token}」`);
  }
  const topBar = read("apps/desktop/src/TopBar.tsx");
  for (const token of ['data-testid="session-pause-toggle"', "data-status", "data-disabled"]) {
    assert(topBar.includes(token), `TopBar 缺少「${token}」`);
  }
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of ['data-testid="paused-notice"', "supportsPauseResume", "onTogglePause"]) {
    assert(workbench.includes(token), `SessionWorkbench 缺少「${token}」`);
  }
  const uiux = read("docs/UI-UX-设计规格.md");
  assert(uiux.includes("`session-pause-toggle`"), "UI-UX 缺少 session-pause-toggle 登记");
  assert(uiux.includes("M7-03"), "UI-UX 缺少 M7-03 登记");
  const plan = read("docs/P1-实施计划与验收标准.md");
  assert(plan.includes("M7-03 会话暂停/恢复"), "P1 计划缺 M7-03 任务节");

  record("静态：状态机/生命周期/D6/适配器/命令/UI/登记逐项", 0);
} catch (error) {
  record(`静态检查失败：${error && error.message ? error.message : error}`, 1);
}

// ===== DoD1：状态机（22 边白名单与单测） =====
record(
  "DoD1 aether-core --lib（会话状态机 22 边；pause/resume 边）",
  run(cargo, ["test", "-p", "aether-core", "--lib"]),
);

// ===== DoD1/2/4/5/6：生命周期集成（10 用例） =====
record(
  "DoD1/2/4/5/6 aether-control m7_03_pause（10 用例）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m7_03_pause"]),
);
record(
  "DoD6 aether-control --lib（错误码/显示全族）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);

// ===== D6 方法表与客户端（adapters 单测） =====
record(
  "D6 aether-adapters --lib（方法表 10 / session.pause / 客户端透传）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]),
);

// ===== DoD3：夹具构建 + 命令层端到端（真实夹具进程） =====
record(
  "构建 aether-adapter-fixture",
  run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]),
);
if (!existsSync(fixture)) {
  record(`夹具产物缺失：${path.relative(repoRoot, fixture)}`, 1);
} else {
  const fixtureEnv = {
    ...process.env,
    AETHER_FIXTURE_BIN: fixture,
    AETHER_REQUIRE_FIXTURE: "1",
  };
  record(
    "DoD3 aether-tauri m7_03_pause（3 用例；能力门/端到端/非法矩阵）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_03_pause"], { env: fixtureEnv }),
  );
}

// ===== DoD3（适配器侧）：四适配器能力声明与 session.pause 处理 =====
record(
  "DoD3 adapter-sdk protocol（10 方法表）",
  vitest("@aether/adapter-sdk", ["src/protocol.test.ts"]),
);
record(
  "DoD3 adapter-mock（capabilities/--no-pause-resume/session.pause）",
  vitest("@aether/adapter-mock", ["src/mock-adapter.test.ts", "src/cli.test.ts"]),
);
record(
  "DoD3 adapter-claude-code（pause_resume/session.pause）",
  vitest("@aether/adapter-claude-code", ["src/claude-adapter.test.ts"]),
);
record(
  "DoD3 adapter-codex（pause_resume/session.pause）",
  vitest("@aether/adapter-codex", ["src/codex-adapter.test.ts"]),
);
record(
  "DoD3 adapter-dsh（pause_resume/session.pause）",
  vitest("@aether/adapter-dsh", ["src/dsh-adapter.test.ts"]),
);

// ===== DoD5：IPC 畸形参数矩阵（session_pause/session_resume） =====
record(
  "DoD5 aether-tauri ipc_validation（含 pause/resume 畸形矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);

// ===== DoD2（UI）：暂停/恢复入口（vitest 5 用例） =====
record(
  "DoD2 UI m7_03_pause_toggle（5 用例：入口/门禁/置灰/错误/空态）",
  vitest("@aether/desktop", ["src/m7_03_pause_toggle.test.tsx"]),
);

// ===== 回归：P0 路径（状态机/取消树/重放/等待态）与命令层 =====
record(
  "回归 aether-control m2_01_lifecycle（10 用例）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"]),
);
record(
  "回归 aether-control m2_05_cancel（5 用例；取消树）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_05_cancel"]),
);
record(
  "回归 aether-control m3_06_retry（重放：与暂停重放共用等价路径）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m3_06_retry"]),
);
record(
  "回归 aether-control m3_12_waiting_state（等待审批置位/回程）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m3_12_waiting_state"]),
);
record(
  "回归 aether-tauri m3_02_session_backend（6 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_02_session_backend"]),
);
record(
  "回归 aether-tauri m3_06_run_retry（夹具会话面；fixture 增量后回归）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_06_run_retry"]),
);
record(
  "回归 前端 SessionWorkbench（5 用例）",
  vitest("@aether/desktop", ["src/SessionWorkbench.test.tsx"]),
);
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对；M7-03 无迁移）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 证据归档 =====
const summary = {
  task: "M7-03",
  stamp,
  platform: process.platform,
  evidence_dir: path.relative(repoRoot, evidenceDir),
  checks,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m7-03] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-03", checks));
