/**
 * M3-12 验证入口：会话等待态写入与等待审批分组（D9；ADR-011 / 计划 v1.18）。
 *
 * 覆盖 DoD：
 *   1) 等待态写入（置位/回程守卫、19 边状态机、单一写者）—— `aether-control --test
 *      m3_12_waiting_state`（dod1_*）+ 本脚本静态守门（`permission.rs` 无会话状态写入调用）；
 *   2) 回程合法性（allow/deny/超时/取消 + run 失败收口；并发幂等）—— `m3_12_waiting_state`
 *      （dod2_*）；
 *   3) 重启 no-op（`restore_pending` 不标记等待态；恢复票据决议/超时无回程）——
 *      `m3_12_waiting_state`（dod3_*）；
 *   4) 多会话并发等待 —— `m3_12_waiting_state`（dod4_*）；
 *   5) 会话列表分组（固定组序/空组隐藏/锚点契约）—— `apps/desktop`（`sessionGroups.test.ts`
 *      + `m3_12_session_groups.test.tsx`）+ 本脚本静态守门（锚点齐备、既有锚点不重命名）；
 *   6) 生产组合路径集成 E2E（工作区基准 + 执行器权限网关 + 真实 PermissionService + Mock
 *      适配器进程）—— `aether-tauri --test m3_12_waiting_state`（证据 JSON 归档）；
 *   7) 回归：M2-05 取消路径、M2-10 零直通、M3-03 队列口径、M3-02 verify 静态检查项
 *      + 前端工作台回归 —— 对应 cargo test / desktop 套件 / 本脚本静态守门。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到
 *       `scripts/test/.tmp/m3-12/evidence-<stamp>/`（Gate 3 逐条出示）。
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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-12");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

// ===== 1. 编译 Mock 适配器单文件（Bun；DoD6 生产组合路径宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；生产组合路径集成 E2E 宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

// ===== 2. aether-control：等待态写入（DoD1–4）=====

record(
  "aether-control --test m3_12_waiting_state（置位/回程守卫 + 事件 from/to + deny/超时/取消/run 失败 + 重启 no-op + 多会话并发）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m3_12_waiting_state", "--", "--nocapture"], {
    cwd: repoRoot,
  }),
);

// ===== 3. aether-tauri：生产组合路径集成 E2E（DoD6）+ 证据归档 =====

mkdirSync(evidenceDir, { recursive: true });
const mockEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_M3_12_EVIDENCE_DIR: evidenceDir,
};
record(
  "aether-tauri --test m3_12_waiting_state（工作区基准 + 执行器网关 + 真实 PermissionService + Mock 进程；waiting_permission 置位/回程 + 零直通）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_12_waiting_state", "--", "--nocapture"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);

// ===== 4. 回归（DoD7）：M2-05 取消路径 / M2-10 零直通 / M3-03 队列口径 =====

record(
  "回归：M2-05 取消路径（m2_05_cancel；含 waiting_permission 取消合法转移）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_05_cancel"], { cwd: repoRoot }),
);
record(
  "回归：M2-10 权限回环零直通（m2_10_permission_loop；Mock 进程）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m2_10_permission_loop"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);
record(
  "回归：M3-03 权限中心（m3_03_permission_center；审批队列口径 + IPC 回环）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_03_permission_center"], {
    cwd: repoRoot,
    env: mockEnv,
  }),
);

// ===== 5. 前端：分组集成 E2E + 工作台回归（DoD5/7）=====

record(
  "pnpm --filter @aether/desktop test（分组出现/消失 E2E + 工作台/权限中心/取消路径回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 6. 静态守门：单一写者/守卫/接线/锚点/约束在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  // 去注释（`//` 与 `/* */`）后检查代码令牌，避免文档注释误伤。
  const stripComments = (source) =>
    source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/.*$/gm, "");
  const permissionRs = read("crates/aether-control/src/permission.rs");
  const permissionCode = stripComments(permissionRs);
  const lifecycleRs = read("crates/aether-control/src/lifecycle.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const sessionState = read("crates/aether-core/src/session_state.rs");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const sessionGroups = read("apps/desktop/src/sessionGroups.ts");
  const topBar = read("apps/desktop/src/TopBar.tsx");

  // DoD1：权限服务不得写会话状态（状态写入唯一入口 = SessionManager）。
  for (const forbidden of ["UpdateSessionStatus", "InsertSession", "SessionManager", "SessionStatus"]) {
    if (permissionCode.includes(forbidden)) {
      problems.push(`permission.rs 不得包含会话状态写入调用/类型：${forbidden}`);
    }
  }

  // DoD1：等待态写入入口/观察者接线/守卫在案。
  const required = [
    [lifecycleRs, "pub async fn mark_waiting_permission", "SessionManager 置位入口"],
    [lifecycleRs, "pub async fn clear_waiting_permission", "SessionManager 回程入口"],
    [lifecycleRs, "impl crate::permission::PendingTicketObserver for SessionManager", "观察者实现"],
    [lifecycleRs, "SessionStatus::WaitingPermission", "等待态守卫"],
    [lifecycleRs, "can_transition", "状态机白名单守卫"],
    [permissionRs, "pub trait PendingTicketObserver", "观察者接口（只上报）"],
    [permissionRs, "fn set_pending_observer", "组合根装配入口"],
    [permissionRs, "fn cleared_session_after_removal", "票据摘除唯一仲裁点判定"],
    [lib, "set_pending_observer", "组合根接线（两阶段装配）"],
    [sessionState, "assert_eq!(legal.len(), 19", "19 边状态机冻结（单测锁定）"],
    // DoD5：分组锚点契约与既有锚点。
    [workbench, 'data-testid="session-group"', "分组容器锚点"],
    [workbench, "data-group={group}", "分组 data-group"],
    [workbench, 'data-testid="session-group-title"', "分组标题锚点"],
    [workbench, 'data-testid="session-list"', "既有会话列表锚点（不重命名）"],
    [workbench, 'data-testid="session-item"', "既有会话项锚点（不重命名）"],
    [workbench, 'data-testid="session-item-status"', "既有状态标签锚点（不重命名）"],
    [workbench, "data-session-id={session.id}", "既有 data-session-id"],
    [workbench, "data-active={String(session.id === activeSessionId)}", "既有 data-active"],
    [sessionGroups, "SESSION_GROUP_ORDER", "固定组序常量"],
    [sessionGroups, '"running"', "组 id running"],
    [sessionGroups, '"waiting_permission"', "组 id waiting_permission"],
    [sessionGroups, '"failed"', "组 id failed"],
    [sessionGroups, '"other"', "组 id other"],
    // M3-02 verify 静态检查项（分组改动触及 SessionWorkbench DOM，须保持）。
    [workbench, "runtime-selector", "M3-02 运行时选择器"],
    [workbench, "capability-badge", "M3-02 能力徽标"],
    [workbench, "VirtualList", "M3-02 消息流虚拟滚动"],
    [workbench, "Markdown", "M3-02 消息流 Markdown"],
    [topBar, "session-status", "M3-02 状态条（并入 TopBar）"],
    [topBar, "run-status", "M3-02 run 状态"],
    [topBar, "interrupt", "M3-02 中断入口"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 固定组序字面量（顺序断言）。
  const orderMatch = /SESSION_GROUP_ORDER[^=]*=\s*\[([\s\S]*?)\]/.exec(sessionGroups);
  const order = orderMatch
    ? [...orderMatch[1].matchAll(/"([a-z_]+)"/g)].map((match) => match[1])
    : [];
  if (order.join(",") !== "running,waiting_permission,failed,other") {
    problems.push(`固定组序必须为 running → waiting_permission → failed → other（实际 ${order}）`);
  }

  // 不新增迁移（M3-12 零 DDL）：等待态写入文件不得含 DDL。
  for (const [source, label] of [
    [permissionRs, "permission.rs"],
    [lifecycleRs, "lifecycle.rs"],
  ]) {
    for (const forbidden of ["CREATE TABLE", "ALTER TABLE", "INSERT INTO sessions", "INSERT INTO runs"]) {
      if (source.includes(forbidden)) {
        problems.push(`${label} 不得包含 DDL（M3-12 不新增迁移）：${forbidden}`);
      }
    }
  }

  // 迁移集不得出现 0004+（M3-12 零迁移；ADR-011 §3.3）。
  const migrationFiles = readdirSync(path.join(repoRoot, "migrations"));
  for (const name of migrationFiles) {
    if (/^000[4-9]/.test(name)) {
      problems.push(`M3-12 不得新增迁移文件：migrations/${name}`);
    }
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：单一写者/守卫/接线/分组锚点/固定组序/零迁移在案",
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 7. T14：生成物幂等（M3-12 无 DTO/命令变更；防漂移）=====

{
  const result = spawnSync(node, [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "generate"], {
    cwd: repoRoot,
    encoding: "utf8",
    env: process.env,
  });
  const changed = /changed=false/.test(result.stdout ?? "");
  if (result.status !== 0 || !changed) {
    console.error(result.stdout ?? "");
    console.error(result.stderr ?? "");
  }
  record(
    "T14 生成物幂等：重新生成 changed=false（M3-12 无 DTO/命令变更；严格 diff 由 CI 执行）",
    result.status === 0 && changed ? 0 : 1,
  );
}

// ===== 8. 证据归档检查（DoD6 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = ["dod6_production_composition.json"];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  console.log(`[m3-12] 证据目录：${evidenceDir}`);
  record("证据归档：生产组合路径集成 E2E 逐用例 JSON", missing.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m3-12", checks));
