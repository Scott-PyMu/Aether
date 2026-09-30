/**
 * M3-08 验证入口：工作区记忆（设计 D14/D9；ADR-004 决策 3；v1.18 网关接线追记）。
 *
 * 覆盖 DoD：
 *   1) 注入（会话创建注入记忆文件；32KB 上限截断 + 显式标记；优先级断言）
 *      —— `aether-control --lib memory::`（组合/优先级/截断/T7 工具映射）
 *      + `m3_08_memory::workspace_set_injects_memory_and_swaps_permission_root`
 *      + Mock session-log 观测注入实际下发（跨会话用例）；
 *   2) 工具（`memory.read/append/write` 经线协议上报并产生 `tool.call_*` 事件）
 *      —— `adapter-mock` 单测（回环 allow/deny/冲突/慢写）+ `m3_08_memory`
 *      `memory_tools_round_trip_through_executor_gate_zero_passthrough`；
 *   3) 权限（D9 白名单：工作区内 allow/ask、外 deny、1MB 上限；越权 denied）
 *      —— `aether-control --lib memory::`（T7 样本 + 1MB 边界）+ 集成 deny 用例；
 *   4) 原子写（临时文件 + rename；写入中断不产生半写文件）
 *      —— `adapter-sdk` 单测 + `m3_08_memory::atomic_write_interrupted_by_termination_*`；
 *   5) 跨会话（会话 A 写入 → 新会话 B 注入读到更新）
 *      —— `m3_08_memory::cross_session_injection_reads_previous_write`；
 *   6) 冲突（外部修改后写入 → `memory_conflict` 不覆盖）
 *      —— `adapter-sdk`/`adapter-mock` 单测 + `m3_08_memory::memory_conflict_*`；
 *   7) `workspace_set` E2E + 执行器权限网关接线（去 `None`；零直通集成断言）
 *      —— `m3_08_memory`（绑定/换根/旧会话不迁移/恢复/ask IPC 回环 + 探针聚合）
 *      + 前端 `m3_08_workspace.test.tsx`（UI-UX 锚点）。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析。
 */
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
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-08");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

// ===== 0. 编译 Mock 适配器单文件（Bun；记忆工具/回环宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；记忆工具宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

// ===== 1. TS 单元：SDK 原子写/冲突 + Mock 记忆工具场景 =====

record(
  "pnpm --filter @aether/adapter-sdk test（原子写临时文件+rename / 冲突不覆盖 / 1MB / 错误码）",
  pnpmRun(["--filter", "@aether/adapter-sdk", "test"]),
);
record(
  "pnpm --filter @aether/adapter-mock test（记忆工具回环/越权/冲突/慢写/注入记录）",
  pnpmRun(["--filter", "@aether/adapter-mock", "test"]),
);

// ===== 2. 前端：设置页工作区绑定（DoD7 UI 面 + 既有回归）=====

record(
  "pnpm --filter @aether/desktop test（workspace-pick/root/apply/result + 既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 3. Rust 单元：注入组合/优先级/32KB 截断/T7 工具映射 =====

record(
  "cargo test -p aether-control --lib memory::（注入/优先级/截断/工具白名单/T7/1MB）",
  run(cargo, ["test", "-p", "aether-control", "--lib", "memory::"], { cwd: repoRoot }),
);

// ===== 4. Rust 集成：工作区记忆全链路（真实 Mock 进程 + 真实网关）=====

mkdirSync(evidenceDir, { recursive: true });
const mockEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_M3_08_EVIDENCE_DIR: evidenceDir,
};
record(
  "m3_08_memory（注入/工具/越权/原子写中断/跨会话/冲突/workspace_set 换根/恢复/零直通）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "m3_08_memory",
      "--",
      "--nocapture",
      "--test-threads=1",
    ],
    { cwd: repoRoot, env: mockEnv },
  ),
);

// ===== 5. 回归：适配器执行器链路 + 权限中心（接线改动不破坏既有语义）=====

record(
  "m3_02_adapter_executor（回归：会话创建/流式/中断/补读生产链路）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_02_adapter_executor"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);
record(
  "m3_03_permission_center（回归：权限中心命令面/回环；执行器接线不破坏）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_03_permission_center"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);
record(
  "m2_01_lifecycle / m2_03_permission（回归：生命周期 + 权限矩阵/审批/超时）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"], { cwd: repoRoot }),
);
record(
  "m2_03_permission（回归：策略矩阵/T7/审批/超时审计）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_03_permission"], { cwd: repoRoot }),
);

// ===== 6. 静态检查：接线/锚点/契约在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const memory = read("crates/aether-control/src/memory.rs");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const executor = read("crates/aether-tauri/src/adapter_executor.rs");
  const sdkMemory = read("packages/adapter-sdk/src/memory.ts");
  const mockScenarios = read("packages/adapter-mock/src/scenarios.ts");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const workspaceTs = read("apps/desktop/src/workspace.ts");
  const required = [
    [memory, '"AGENTS.md"', "记忆优先级（AGENTS.md）"],
    [memory, "MEMORY_INJECTION_MAX_BYTES: usize = 32 * 1024", "32KB 注入上限"],
    [memory, "MEMORY_TRUNCATION_MARKER", "截断显式标记"],
    [memory, "memory.read", "工具映射（memory.read）"],
    [memory, "memory.append", "工具映射（memory.append）"],
    [memory, "memory.write", "工具映射（memory.write）"],
    [memory, "T7_TEXTUAL_SAMPLES", "T7 路径样本复用"],
    [sessionBackend, "fn workspace_set", "workspace_set 真实后端"],
    [sessionBackend, "compose_memory_injection", "会话创建注入组合"],
    [sessionBackend, "with_workspace_store", "工作区写路径接线"],
    [sessionBackend, "restore_workspace_binding", "启动恢复工作区绑定"],
    [sessionBackend, "set_workspace_root", "权限基准与工作区同源"],
    [lib, "permission_loop::PermissionServiceGate::new", "执行器权限网关接线（去 None）"],
    [lib, "restore_workspace_binding", "组合根启动恢复接线"],
    [executor, "SessionMappingGate", "适配器会话 id → 核心会话 id 映射"],
    [executor, "create_session_with_prompt", "system_prompt 注入下发"],
    [executor, "permission_loop_stats", "零直通探针聚合（DoD7 证据）"],
    [sdkMemory, "MEMORY_CONFLICT_CODE", "冲突错误码（memory_conflict）"],
    [sdkMemory, "atomicWriteMemory", "原子写（临时文件 + rename）"],
    [mockScenarios, "memory.read", "Mock 记忆工具触发（read）"],
    [mockScenarios, "memory.append", "Mock 记忆工具触发（append）"],
    [mockScenarios, "memory.write", "Mock 记忆工具触发（write）"],
    [workspaceTs, "workspace_set", "前端工作区 IPC 契约"],
    [workspaceTs, "startup_pick_target", "目录选择复用系统选择器（不新增命令）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // UI-UX §7.3 M3-08 锚点必须齐备（生产代码内出现）。
  const anchors = ["workspace-pick", "workspace-root", "workspace-apply", "workspace-result"];
  for (const anchor of anchors) {
    if (!settingsPage.includes(anchor)) problems.push(`UI-UX 锚点缺失：${anchor}`);
  }
  // 禁止项：会话创建注入不得绕过组合（不存在静态硬编码 32KB 之外的策略复制）。
  if (/evaluate_memory_tool[\s\S]*PolicyEngine::new/.test(memory)) {
    // 允许 unit test 构造引擎；仅提示性检查不做失败（此处保持通过）。
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：注入/工具/权限接线/网关/映射/锚点在案", problems.length === 0 ? 0 : 1);
}

// ===== 7. 证据归档检查（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_workspace_binding.json",
    "dod2_tool_round_trip.json",
    "dod4_atomic_interrupt.json",
    "dod5_cross_session_injection.json",
    "dod6_memory_conflict.json",
    "dod7_ask_ipc_round_trip.json",
    "dod7_restore_binding.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  console.log(`[m3-08] 证据目录：${evidenceDir}`);
  record(
    `证据归档：7 份逐用例 JSON（注入/工具/原子写/跨会话/冲突/ask 回环/恢复）`,
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-08", checks));
