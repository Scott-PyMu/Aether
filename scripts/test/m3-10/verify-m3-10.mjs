/**
 * M3-10 验证入口：思考深度（会话级参数；设计 D4/D6/D7；ADR-010 决策 2/4）。
 *
 * 覆盖 DoD：
 *   1) 迁移 0003 列断言；0001→0003 幂等；历史 run 行为 NULL
 *      —— `aether-store --test m3_10_thinking`（dod1_*）+ `verify:m1-03`
 *      （迁移集有效 schema ↔ 设计文档附录 C 逐项一致；本地专属）；
 *   2) 透传（会话级/覆盖/缺省 → 适配器回显；runs 落生效值；sessions 一致）
 *      —— `aether-tauri --test m3_10_thinking`（dod2_passthrough；真实 Mock 进程）
 *      + `adapter-mock` 单测（技能声明 + session-log 观测）；
 *   3) 能力门（同步警告 / 延迟判定 / 字段不透传 / UI 置灰 + tooltip）
 *      —— `m3_10_thinking`（dod3_sync_gate / dod3_delayed_gate）
 *      + 前端 `m3_10_thinking.test.tsx`（滑块 data-enabled/置灰/tooltip/警告）；
 *   4) 重放（按会话级值恢复；run 启动重新判定）
 *      —— `m3_10_thinking`（dod4_replay）+ `aether-control --test m3_06_retry`（回归）；
 *   5) 校验矩阵（0–4 合法；5/-1 越界；1.5/"高" 类型非法；不落库/不透传）
 *      —— `ipc_validation`（thinking_depth_matrix_valid_and_invalid_samples）；
 *   6) 三官方适配器能力声明与档位映射记录（文档化）——`docs/M3-10-证据.md`
 *      + 静态检查（能力项/档位常量/三适配器接线在案；契约与附录 C/ADR-010 一致）；
 *      Claude Code `MAX_THINKING_TOKENS` / Codex `model_reasoning_effort` /
 *      DSH ACP `session/set_config_option`（`reasoning_effort`）；
 *   7) T14 生成物更新（`SessionSummary.thinking_depth`、`warnings`、`SessionWarning`）
 *      —— 重新生成幂等（changed=false）+ 生成物内容断言；严格
 *      `git diff --exit-code` 由提交后的 CI（scripts/ci/bindings.mjs check）执行。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到
 *       `scripts/test/.tmp/m3-10/evidence-<stamp>/`（Gate 3 逐条出示）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-10");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

// ===== 0. 编译 Mock 适配器单文件（Bun；能力声明/观测宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；thinking_depth 能力与观测宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

// ===== 1. TS 单元：Mock 能力声明/观测 + 官方适配器档位映射 + SDK 回归 =====

record(
  "pnpm --filter @aether/adapter-mock test（能力声明/--no-thinking-depth/透传观测 + 既有回归）",
  pnpmRun(["--filter", "@aether/adapter-mock", "test"]),
);
record(
  "pnpm --filter @aether/adapter-claude-code test（档位→MAX_THINKING_TOKENS 映射与注入）",
  pnpmRun(["--filter", "@aether/adapter-claude-code", "test"]),
);
record(
  "pnpm --filter @aether/adapter-codex test（档位→model_reasoning_effort 映射与注入）",
  pnpmRun(["--filter", "@aether/adapter-codex", "test"]),
);
record(
  "pnpm --filter @aether/adapter-dsh test（档位→ACP reasoning_effort 映射与设置）",
  pnpmRun(["--filter", "@aether/adapter-dsh", "test"]),
);
record(
  "pnpm --filter @aether/adapter-sdk test（回归：帧/协议/记忆工具未被本任务破坏）",
  pnpmRun(["--filter", "@aether/adapter-sdk", "test"]),
);

// ===== 2. 前端：思考深度滑块 E2E + 既有回归 =====

record(
  "pnpm --filter @aether/desktop test（滑块置灰/透传/警告/回显 + 既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 3. Rust 单元/契约：实体 ↔ 有效 schema（含 0003 增量列）=====

record(
  "aether-core --test m1_02_contract（实体字段 ↔ 0001+0002+0003 有效 schema）",
  run(cargo, ["test", "-p", "aether-core", "--test", "m1_02_contract"], { cwd: repoRoot }),
);

// ===== 4. Rust 存储层：迁移 0003 列/幂等/历史 NULL + 单写队列往返 =====

record(
  "aether-store --test m3_10_thinking（列断言/幂等/历史 NULL/读写往返/改写命令）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_10_thinking"], { cwd: repoRoot }),
);

// ===== 5. 控制层回归：创建/发送/重放路径（会话级值解析）=====

record(
  "aether-control --test m2_01_lifecycle（回归：状态机/run 串行/幂等/ack/断流）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"], { cwd: repoRoot }),
);
record(
  "aether-control --test m3_06_retry（回归：重放准入/旧 run 保留/Mode R/N）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m3_06_retry"], { cwd: repoRoot }),
);

// ===== 6. Rust 集成：真实 Mock 进程（透传/能力门/重放）+ 校验矩阵 =====

mkdirSync(evidenceDir, { recursive: true });
const mockEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_M3_10_EVIDENCE_DIR: evidenceDir,
};
record(
  "m3_10_thinking（透传/同步与延迟能力门/重放；真实 Mock 进程）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "m3_10_thinking",
      "--",
      "--nocapture",
      "--test-threads=1",
    ],
    { cwd: repoRoot, env: mockEnv },
  ),
);
record(
  "ipc_validation（校验矩阵：0–4 合法；5/-1 → out_of_range；1.5/\"高\" → invalid_type；不落库/不透传）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);
record(
  "m3_02_adapter_executor（回归：执行器会话创建/流式/中断链路；默认档位透传不破坏）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_02_adapter_executor"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);

// ===== 7. 迁移集 ↔ 设计文档附录 C（含 0003 列；ADR-010 冻结文本）=====
//
// 设计文档为本地基线（`.gitignore` 排除，不入库）：CI 上无该文件，此时跳过
// 附录 C 比对（0003 列断言已由 m3_10_thinking 覆盖）；本地保留完整比对。

const designDoc = path.join(repoRoot, "设计文档.md");
if (existsSync(designDoc)) {
  record(
    "verify:m1-03（迁移集有效 schema ↔ 附录 C 逐项一致；0001/0002/0003 未改）",
    run(node, [path.join(repoRoot, "scripts", "test", "m1-03", "verify-m1-03.mjs")]),
  );
} else {
  checks.push({
    name: "verify:m1-03（迁移集 ↔ 附录 C；设计文档未入库，本地专属）",
    skip: true,
    reason: "设计文档.md 不在仓库（.gitignore）；0003 列断言由 m3_10_thinking 覆盖",
  });
}

// ===== 8. T14：生成物幂等 + 内容断言 =====

{
  const result = spawnSync(
    node,
    [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "generate"],
    { cwd: repoRoot, encoding: "utf8", env: process.env },
  );
  const changed = /changed=false/.test(result.stdout ?? "");
  if (result.status !== 0 || !changed) {
    console.error(result.stdout ?? "");
    console.error(result.stderr ?? "");
  }
  record(
    "T14 生成物幂等：重新生成 changed=false（严格 git diff --exit-code 由提交后的 CI 执行）",
    result.status === 0 && changed ? 0 : 1,
  );
}

// ===== 9. 静态守门：契约/接线/能力门/锚点/边界在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const coreDomain = read("crates/aether-core/src/domain.rs");
  const storeOps = read("crates/aether-store/src/ops.rs");
  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  const executor = read("crates/aether-tauri/src/adapter_executor.rs");
  const dto = read("crates/aether-tauri/src/ipc/dto.rs");
  const validate = read("crates/aether-tauri/src/ipc/validate.rs");
  const backend = read("crates/aether-tauri/src/session_backend.rs");
  const sessionClient = read("crates/aether-adapters/src/session_client.rs");
  const mockAdapterTs = read("packages/adapter-mock/src/mock-adapter.ts");
  const mockCli = read("packages/adapter-mock/src/cli.ts");
  const claudeCli = read("packages/adapter-claude-code/src/claude-cli.ts");
  const claudeAdapter = read("packages/adapter-claude-code/src/claude-adapter.ts");
  const codexAdapter = read("packages/adapter-codex/src/codex-adapter.ts");
  const dshAdapter = read("packages/adapter-dsh/src/dsh-adapter.ts");
  const generated = read("packages/protocol/src/bindings.ts");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const sessionTs = read("apps/desktop/src/session.ts");
  const migration = read("migrations/0003_p0_ui_extensions.sql");

  const required = [
    // 核心模型：档位常量/能力项/会话与 run 列。
    [coreDomain, "THINKING_DEPTH_CAPABILITY", "能力项常量（thinking_depth）"],
    [coreDomain, "THINKING_DEPTH_DEFAULT", "缺省档位常量（2）"],
    [coreDomain, "pub thinking_depth: u8", "Session.thinking_depth 域字段"],
    [coreDomain, "pub thinking_depth: Option<u8>", "Run.thinking_depth 域字段"],
    // 存储：列写入/读取与能力门改写命令（单写队列）。
    [storeOps, "thinking_depth", "sessions/runs 列读写"],
    [storeOps, "UpdateSessionThinkingDepth", "延迟能力门会话改写命令"],
    [storeOps, "UpdateRunThinkingDepth", "run 生效值改写命令"],
    // 控制层：显式档位的创建/发送与重放读会话级值。
    [lifecycle, "create_session_with_depth", "会话级档位创建路径"],
    [lifecycle, "send_with_thinking_depth", "run 覆盖发送路径"],
    [lifecycle, "thinking_override", "覆盖/会话级透传标记"],
    // 执行器：能力门判定与字段不透传。
    [executor, "connection_supports_thinking_depth", "能力门判定（hello capabilities）"],
    [executor, "THINKING_DEPTH_CAPABILITY", "执行器能力项引用"],
    [executor, "send_with_thinking_depth", "覆盖透传 session.send"],
    [executor, "UpdateSessionThinkingDepth", "延迟判定会话改写（ADR-010）"],
    // IPC：请求字段/校验/警告码。
    [dto, "pub thinking_depth: Option<i64>", "session_create/session_send 可选字段"],
    [dto, "thinking_depth_value", "i64 → u8 越界校验"],
    [validate, "pub fn validate_thinking_depth", "档位 0–4 校验函数"],
    [backend, "thinking_depth_unsupported", "警告码（ADR-006 附录 B 子表）"],
    [backend, "pub struct SessionWarning", "警告 DTO（warnings[]）"],
    [backend, "with_warnings", "session_create 同步警告交付"],
    // 适配器客户端：协议字段透传。
    [sessionClient, "send_with_thinking_depth", "session.send 覆盖透传"],
    [sessionClient, "thinking_depth", "session.create/send 协议字段"],
    // Mock：能力声明与观测（缺省支持；--no-thinking-depth 负向夹具）。
    [mockAdapterTs, "thinkingDepthUnsupported", "未支持能力夹具开关"],
    [mockAdapterTs, "thinking_depth", "session-log 透传观测"],
    [mockCli, "--no-thinking-depth", "CLI 负向夹具开关"],
    // 官方适配器档位映射（M3-10 DoD6；三运行时声明与映射同源，映射表见证据文档）。
    [claudeAdapter, "thinking_depth", "Claude Code 能力声明"],
    [claudeCli, "MAX_THINKING_TOKENS", "Claude Code 预算注入"],
    [claudeCli, "CLAUDE_THINKING_TOKEN_BUDGET", "Claude Code 档位映射表"],
    [codexAdapter, "thinking_depth", "Codex 能力声明"],
    [codexAdapter, "model_reasoning_effort", "Codex 档位映射（reasoning effort）"],
    [codexAdapter, "CODEX_REASONING_BY_DEPTH", "Codex 档位映射表"],
    [dshAdapter, "thinking_depth", "DSH 能力声明（ACP configOptions）"],
    [dshAdapter, "DSH_REASONING_CONFIG_ID", "DSH `reasoning_effort` 配置项 id"],
    [dshAdapter, "dshReasoningValueForDepth", "DSH 档位 → effort 映射（按广告值定档）"],
    [dshAdapter, "session/set_config_option", "DSH ACP 档位设置调用"],
    [dshAdapter, "appliedReasoningValue", "DSH 生效值同值跳过（覆盖回位自愈）"],
    // T14 生成物（禁止手改，仅断言内容存在）。
    [generated, "SessionWarning", "生成物 SessionWarning"],
    [generated, "thinking_depth", "生成物 thinking_depth 字段"],
    // 前端契约与锚点。
    [sessionTs, "THINKING_DEPTH_CAPABILITY", "前端能力项常量"],
    [sessionTs, "thinking_depth", "前端请求/回显字段"],
    [workbench, "thinking-slider", "UI-UX 锚点（thinking-slider）"],
    [workbench, "thinking-disabled-hint", "UI-UX 锚点（thinking-disabled-hint）"],
    [workbench, "thinking-popover", "UI-UX 锚点（thinking-popover）"],
    [workbench, "data-enabled={thinkingSupported}", "能力置灰 data-enabled"],
    [workbench, "THINKING_DEPTH_UNSUPPORTED_HINT", "置灰 tooltip 文案"],
    // 迁移文本（ADR-010 附录 A 冻结）。
    [migration, "ALTER TABLE sessions ADD COLUMN thinking_depth INTEGER NOT NULL DEFAULT 2", "sessions 列"],
    [migration, "ALTER TABLE runs ADD COLUMN thinking_depth INTEGER", "runs 列"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 边界守门：不新增事件类型（附录 B 不变）——0003 迁移不得含事件枚举变更。
  if (/ALTER TABLE events|CREATE TABLE events/.test(migration)) {
    problems.push("迁移 0003 不得改动 events 表（不新增事件类型；ADR-010 附录 B 不变）");
  }
  // 执行器能力门：未支持时不得透传（`(None, None)` 分支在案）。
  const gateBody = /fn execute_inner[\s\S]*?ensure_adapter_session/.exec(executor)?.[0] ?? "";
  if (!gateBody.includes("!supported")) {
    problems.push("执行器能力门：未支持分支缺失（字段不透传 + 落缺省 2）");
  }
  // 前端透传：session_create / session_send 均携带 thinking_depth。
  const createCalls = (workbench.match(/thinking_depth: thinkingDepth/g) ?? []).length;
  if (createCalls < 2) {
    problems.push(`前端透传缺失：session_create/session_send 应各携带 thinking_depth（实际 ${createCalls}）`);
  }
  // DSH：ACP `session/new|resume` 广告 `reasoning_effort`（category=thought_level）→ 声明能力，
  // 经 `session/set_config_option` 映射档位；声明与接线必须同源（声明了能力却没有设置调用 = 断言失败）。
  const dshCapabilities =
    /RUNTIME_CAPABILITIES[\s\S]*?\];/.exec(dshAdapter)?.[0] ?? "";
  if (!dshCapabilities.includes("thinking_depth")) {
    problems.push("DSH 适配器应声明 thinking_depth（ACP reasoning_effort 面已支持；ADR-010 决策 2）");
  }
  if (!/applyReasoning\(session, thinkingDepth, "session\.create"\)/.test(dshAdapter)) {
    problems.push("DSH 会话级档位未接线：session.create 路径缺少 reasoning_effort 应用调用");
  }
  if (
    !/await this\.applyReasoning\(\s*session,\s*thinkingOverride \?\? session\.thinkingDepth,/.test(
      dshAdapter,
    )
  ) {
    problems.push("DSH run 档位未接线：session.send 生效值应为「覆盖 ?? 会话级」（ACP 设置为会话级有状态）");
  }
  if (/session\.thinkingDepth\s*=\s*thinkingOverride/.test(dshAdapter)) {
    problems.push("DSH 覆盖回写会话级（违反 ADR-010「session.send 覆盖仅本次 run」）");
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：模型/存储/控制/执行器/IPC/Mock/生成物/锚点/边界在案",
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 10. 证据归档检查（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod2_passthrough.json",
    "dod3_sync_gate.json",
    "dod3_delayed_gate.json",
    "dod4_replay.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  console.log(`[m3-10] 证据目录：${evidenceDir}`);
  record(
    "证据归档：4 份逐用例 JSON（透传/同步能力门/延迟能力门/重放）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-10", checks));
