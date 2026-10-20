#!/usr/bin/env node
/**
 * M7-08 验证入口：工作区管理与多工作区（ADR-026 §2.1；ADR-014 R5/R15 不变量 11；
 * 直接实施项，不构成 P1 门禁）。
 *
 * 覆盖：
 * - DoD1：五命令登记 + 参数校验矩阵（`ipc_validation`：未知字段/缺参/非 ULID/
 *   超长名称/路径拒绝）+ bindings 生成物含五命令（静态）+ 单写队列写路径（store 测试）；
 * - DoD2：权限基准按会话创建固定（`m7_08_workspaces::reassign_switches_memory_but_keeps_creation_baseline`）：
 *   `workspace_set` 换根仅新会话生效、改派不重算（A 基准内 allow / B 基准外 ask）、
 *   跨重启从 `sessions.config` 快照恢复；基准快照配置键单测（`aether-control --lib permission::`）；
 * - DoD3：删除安全（引用 → `workspace_in_use` 不删行/不级联；只删行不删磁盘；审计）；
 * - DoD4：`workspaces.memory_files` 恒空数组（store 测试）；
 * - DoD5：左栏树（`m7_08_workspace_tree.test.tsx`：分组/计数/折叠/创建/改名/删除/改派 +
 *   常驻基准提示 + 入口去重）+ UI-UX §7.3 锚点登记（静态）；
 * - DoD6：M3-08 `workspace_set` E2E 新口径复跑（换根仅新会话生效，R15）+
 *   M3-12 分组回归 + 全量前端/工作区回归；`verify:m1-03`（M7-08 不新增迁移；
 *   基准快照落 `sessions.config` JSON 键）。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件（M3-08/M3-12 回归）；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-08");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 0. 编译 Mock 适配器单文件（M3-08/M3-12 回归复用）=====
record(
  "编译 Mock 适配器单文件（bun build --compile；M3-08/M3-12 回归夹具）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

const mockEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
};

// ===== 1. 静态检查（一次 try/catch → 单条记录）=====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // 1) 命令集登记（collect_commands + 两个 generate_handler + DTO/后端/错误码）。
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  for (const name of [
    "workspaces_list",
    "workspace_create",
    "workspace_update",
    "workspace_delete",
    "session_workspace_assign",
  ]) {
    assert(commands.includes(`${name},`), `commands.rs 缺 ${name} 注册`);
  }
  const collectedBlock = /collect_commands!\[([\s\S]*?)\]/.exec(commands)?.[1] ?? "";
  const handlerBlocks = [...commands.matchAll(/generate_handler!\[([\s\S]*?)\]/g)].map(
    (match) => match[1],
  );
  assert(handlerBlocks.length === 2, "generate_handler 必须恰有两处（debug/release）");
  for (const block of [collectedBlock, ...handlerBlocks]) {
    for (const name of [
      "workspaces_list",
      "workspace_create",
      "workspace_update",
      "workspace_delete",
      "session_workspace_assign",
    ]) {
      assert(new RegExp(`\\b${name}\\b`).test(block), `命令面缺 ${name}`);
    }
  }
  assert(commands.includes('WorkspaceCreateRequest'), "workspace_create DTO 未接线");
  const backend = read("crates/aether-tauri/src/ipc/backend.rs");
  for (const name of [
    "workspaces_list",
    "workspace_create",
    "workspace_update",
    "workspace_delete",
    "session_workspace_assign",
  ]) {
    assert(backend.includes(`fn ${name}`), `IpcBackend 缺 ${name}`);
  }

  // 2) 错误码登记（ADR-006 附录 B：workspace_not_found / workspace_in_use）。
  const errorRs = read("crates/aether-tauri/src/ipc/error.rs");
  for (const code of ["workspace_not_found", "workspace_in_use"]) {
    assert(errorRs.includes(`"${code}"`), `错误码缺 ${code}`);
  }
  assert(
    errorRs.includes("WorkspaceNotFound") && errorRs.includes("WorkspaceInUse"),
    "IpcErrorCode 枚举缺工作区两码",
  );

  // 3) bindings 生成物（禁止手改；五命令 + 请求类型）。
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const name of [
    "workspaces_list",
    "workspace_create",
    "workspace_update",
    "workspace_delete",
    "session_workspace_assign",
  ]) {
    assert(bindings.includes(`__TAURI_INVOKE("${name}"`), `bindings 缺 ${name}`);
  }
  for (const typeName of [
    "WorkspacesListRequest",
    "WorkspaceCreateRequest",
    "WorkspaceUpdateRequest",
    "WorkspaceDeleteRequest",
    "SessionWorkspaceAssignRequest",
  ]) {
    assert(bindings.includes(`export type ${typeName}`), `bindings 缺类型 ${typeName}`);
  }

  // 4) 存储写路径（单写队列）+ 引用检查在事务内 + 计数读路径。
  const ops = read("crates/aether-store/src/ops.rs");
  for (const token of [
    "RenameWorkspace",
    "DeleteWorkspace",
    "AssignSessionWorkspace",
    "WorkspaceInUse",
    "workspace_session_counts",
    "session_has_active_run",
  ]) {
    assert(ops.includes(token), `store 缺 ${token}`);
  }
  const deleteQuery = "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1";
  assert(ops.includes(deleteQuery), "删除前置引用检查必须在同一事务内");

  // 5) R15：权限基准按会话创建固定（双引擎 + 会话基准解析 + 配置快照键）。
  const permission = read("crates/aether-control/src/permission.rs");
  for (const token of [
    "SESSION_CONFIG_BASELINE_SECTION",
    "SESSION_CONFIG_BASELINE_KEY",
    "config_with_session_baseline",
    "session_baseline_from_config",
    "set_session_baseline",
    "base_policy",
    "session_baselines",
    "baseline_engines",
    "换根仅对新会话生效",
  ]) {
    assert(permission.includes(token), `permission.rs 缺「${token}」`);
  }
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  for (const token of [
    "persist_session_baseline",
    "session.workspace_assigned",
    "replace_workspace_memory_block",
    "workspace_in_use",
    "disk_untouched",
    "memory_files: Vec::new()",
    "权限基准不变，如需变更请新建会话",
  ]) {
    assert(sessionBackend.includes(token), `session_backend.rs 缺「${token}」`);
  }

  // 6) UI：左栏树锚点/入口/接线 + 设置页入口去重。
  const tree = read("apps/desktop/src/WorkspaceTree.tsx");
  for (const token of [
    'data-testid="workspace-tree"',
    'data-testid="workspace-item"',
    'data-testid="workspace-item-toggle"',
    'data-testid="workspace-item-name"',
    'data-testid="workspace-item-count"',
    'data-testid="workspace-session"',
    'data-testid="workspace-create"',
    'data-testid="workspace-create-form"',
    'data-testid="workspace-name-input"',
    'data-testid="workspace-root-pick"',
    'data-testid="workspace-rename"',
    'data-testid="workspace-delete"',
    'data-testid="workspace-delete-confirm"',
    'data-testid="workspace-assign"',
    'data-testid="workspace-assign-notice"',
    "WORKSPACE_BASELINE_NOTICE",
    "UNASSIGNED_WORKSPACE_KEY",
  ]) {
    assert(tree.includes(token), `WorkspaceTree 缺「${token}」`);
  }
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of ["<WorkspaceTree", "workspacesIpc", "onAssigned"]) {
    assert(workbench.includes(token), `SessionWorkbench 缺「${token}」`);
  }
  const workspaceTs = read("apps/desktop/src/workspace.ts");
  for (const token of [
    "export interface WorkspacesIpc",
    "export interface WorkspaceSummary",
    "SessionWorkspaceAssignResult",
    "assignSession",
    "export const workspacesIpc",
  ]) {
    assert(workspaceTs.includes(token), `workspace.ts 缺「${token}」`);
  }
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  for (const token of ["settings-workspace-entry", "已有会话不迁移", "缺省工作区"]) {
    assert(settingsPage.includes(token), `SettingsPage 缺「${token}」`);
  }

  // 7) UI-UX §7.3 锚点登记 + 计划状态 + 验证入口 + 证据归档。
  const uiux = read("docs/UI-UX-设计规格.md");
  for (const token of [
    "`workspace-tree`",
    "`workspace-item`",
    "`workspace-create`",
    "workspace-item-count",
    "workspace-assign",
    "settings-workspace-entry",
    "工作区实现登记（2026-10-20）",
  ]) {
    assert(uiux.includes(token), `UI-UX 规格缺「${token}」`);
  }
  const plan = read("docs/P1-实施计划与验收标准.md");
  assert(plan.includes("#### M7-08 工作区管理与多工作区"), "P1 计划缺 M7-08 任务节");
  assert(plan.includes("执行状态：✅ 已完成（2026-10-20）"), "P1 计划缺 M7-08 执行状态");
  const rootPackage = JSON.parse(read("package.json"));
  assert(
    rootPackage.scripts?.["verify:m7-08"] === "node scripts/test/m7-08/verify-m7-08.mjs",
    "package.json 缺 verify:m7-08 脚本",
  );
  assert(existsSync(path.join(repoRoot, "docs/M7-08-证据.md")), "docs/M7-08-证据.md 缺失");

  // 8) 实现级存储口径：不新增迁移（基准快照落 sessions.config JSON 键）。
  const migrationNames = readdirSync(path.join(repoRoot, "migrations"));
  assert(
    !migrationNames.some((name) => /workspace|baseline/i.test(name)),
    "M7-08 不新增迁移（权限基准快照落 sessions.config JSON 键）",
  );
  record("静态：命令面/DTO/错误码/bindings/存储/R15/UI 锚点/计划登记逐项", 0);
} catch (error) {
  record(`静态检查失败：${error && error.message ? error.message : error}`, 1);
}

// ===== 2. DoD1/DoD3/DoD4（存储）：写路径/计数/删除安全/改派原子写 =====
record(
  "DoD1/3/4 cargo test -p aether-store --test m7_08_workspaces（5 用例）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m7_08_workspaces"], {
    cwd: repoRoot,
  }),
);

// ===== 3. DoD1/DoD2/DoD3/DoD5（后端集成）：改派基准固定/删除安全/计数/穷举矩阵 =====
record(
  "DoD1/2/3/5 cargo test -p aether-tauri --test m7_08_workspaces（6 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_08_workspaces"], {
    cwd: repoRoot,
  }),
);

// ===== 4. DoD2（控制层）：基准快照配置键 roundtrip 单测 =====
record(
  "DoD2 cargo test -p aether-control --lib permission::（基准快照键/数据目录区分）",
  run(cargo, ["test", "-p", "aether-control", "--lib", "permission::"], { cwd: repoRoot }),
);

// ===== 5. DoD1：IPC 参数校验矩阵（含五命令畸形样本 + 合法样本到达后端） =====
record(
  "DoD1 cargo test -p aether-tauri --test ipc_validation（含工作区五命令矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);

// ===== 6. DoD5（前端）：工作区树 =====
record(
  "DoD5 前端 m7_08_workspace_tree（9 用例：渲染/计数/折叠/创建/改名/删除/改派/错误）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m7_08_workspace_tree.test.tsx",
  ]),
);

// ===== 7. DoD6：M3-08 workspace_set E2E 新口径 + M3-12 分组回归 =====
record(
  "DoD6 M3-08 E2E 复跑（workspace_set 换根仅新会话生效，R15；真实 Mock）",
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
record(
  "DoD6 M3-12 回归（分组/等待态 + workspace_set 换根基线，真实 Mock）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_12_waiting_state"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);
record(
  "回归 m6_01_rules（会话档位 + 规则；未登记基准会话保持 P0 缺省语义）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m6_01_rules"], { cwd: repoRoot }),
);
record(
  "回归 m2_03_permission / m6_01_permission_modes（权限矩阵/审批；会话基准兼容）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_03_permission"], { cwd: repoRoot }),
);
record(
  "回归 m6_01_permission_modes（三档矩阵/规则/always）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m6_01_permission_modes"], {
    cwd: repoRoot,
  }),
);

// ===== 8. 回归：全量前端 + typecheck + M3-08 设置页工作区绑定 =====
record(
  "回归 @aether/desktop 全量（含 m3_08_workspace 设置页绑定）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "test"]),
);
record(
  "回归 @aether/desktop typecheck（tsc --noEmit）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);

// ===== 9. 回归：迁移 ↔ 附录 C（M7-08 不新增迁移） =====
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对；M7-08 无 schema 变更）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 证据归档 =====
const summary = {
  task: "M7-08",
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
console.log(`[m7-08] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-08", checks));
