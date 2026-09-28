/**
 * M3-03 验证入口：权限中心与运行状态面板（设计 D9/D5；UI-UX S-03/S-04）。
 *
 * 覆盖 DoD：
 *   1) E2E：ask 弹窗（审批卡）→ 允许/拒绝 → 适配器收到决议；超时 deny 提示与审计可查
 *      —— 前端集成（`m3_03_permission_center.test.tsx`：清单/对照/决策/队列/超时呈现）
 *      + 真实适配器链路（`m3_03_permission_center.rs`：真实 Mock 进程 + 真实网关 +
 *      `SessionBackend` IPC 命令 → 适配器收到决议，零直通；覆盖 M2-10 异常路径：
 *      deny / 超时 deny / once / session / 重启后 pending）；
 *   2) 同会话并发 ask 排队展示（≤1 激活）—— 前端集成（1 激活 + 排队计数）+
 *      IPC 清单形状（3 条并发 ask 全量返回、决议后逐条清空）；
 *   3) 只读模式/降级横幅可渲染（标志注入）—— 前端集成（healthBus 注入 + 横幅/存储指示/
 *      发送禁用）；disabled 适配器不可创建会话 —— 前端（选择器禁用 + 兜底拒绝）+
 *      后端防线（`session_create` 拒绝 + 不落库）。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-03");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

// ===== 1. 编译 Mock 适配器单文件（Bun；真实回环宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；真实回环宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

// ===== 2. 前端：权限中心/状态面板交互（DoD1/2/3 + 既有回归）=====

record(
  "pnpm --filter @aether/desktop test（审批卡/对照/决策/队列/超时/降级/disabled/顶栏 + 既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 3. Rust：命令面 + 真实适配器回环（DoD1/2/3）=====

mkdirSync(evidenceDir, { recursive: true });
record(
  "m3_03_permission_center（IPC 清单/决议 → 适配器收到决议；deny/超时/once/session/重启 pending；并发清单；disabled 拒建会话）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_03_permission_center", "--", "--nocapture"],
    {
      env: {
        AETHER_MOCK_ADAPTER: mockAdapter,
        AETHER_REQUIRE_MOCK_ADAPTER: "1",
        AETHER_M3_03_EVIDENCE_DIR: evidenceDir,
      },
    },
  ),
);

// ===== 4. 静态检查：接线与契约在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const shutdown = read("crates/aether-tauri/src/shutdown.rs");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const panel = read("apps/desktop/src/PermissionPanel.tsx");
  const runtimePanel = read("apps/desktop/src/RuntimePanel.tsx");
  const topBar = read("apps/desktop/src/TopBar.tsx");
  const permission = read("apps/desktop/src/permission.ts");
  const projection = read("apps/desktop/src/permissionProjection.ts");
  const required = [
    [sessionBackend, "fn permissions_pending", "permissions_pending 真实后端"],
    [sessionBackend, "fn permission_resolve", "permission_resolve 真实后端"],
    [sessionBackend, "with_permissions", "权限服务注入接口"],
    [sessionBackend, "canonical_target", "原文/规范化对照字段"],
    [sessionBackend, "已禁用", "disabled 运行时拒建会话（后端防线）"],
    [lib, "PermissionService::new", "组合根构造权限服务"],
    [lib, "restore_pending", "D9：核心重启后待审批恢复"],
    [lib, "spawn_background", "D9：300s 超时巡检"],
    [lib, "with_permissions", "权限服务注入 SessionBackend"],
    [shutdown, "install_permission_service", "退出序列停止权限巡检"],
    [permission, "pendingPermissions", "前端权限 IPC 契约"],
    [permission, "resolvePermission", "前端决议 IPC 契约"],
    [permission, "retryRuntime", "runtime_retry IPC 契约"],
    [permission, "enableRuntime", "runtime_enable IPC 契约"],
    [projection, "targetsEquivalent", "原文/规范化等价判定（data-equal）"],
    [panel, "permission-card", "审批卡锚点"],
    [panel, "permission-target-raw", "原文 target 展示"],
    [panel, "permission-target-canonical", "规范化结果展示"],
    [panel, "permission-timeout-note", "300s 超时提示"],
    [panel, "permission-allow-once", "仅本次允许"],
    [panel, "permission-allow-session", "本会话允许"],
    [panel, "permission-deny", "拒绝"],
    [panel, "权限门仅约束经线协议上报的工具调用", "D9 边界声明（C6）"],
    [panel, "aria-modal", "非模态审批卡（Q3）"],
    [runtimePanel, "runtime-retry", "运行时重试入口"],
    [runtimePanel, "runtime-enable", "运行时重新启用入口"],
    [runtimePanel, "version_mismatch", "禁止直接启用的修复说明"],
    [topBar, "runtime-badge", "顶栏运行时徽标"],
    [topBar, "storage-indicator", "顶栏存储指示"],
    [topBar, "pending-permission-badge", "待审批计数徽标"],
    [workbench, "PermissionPanel", "右栏权限面板接线"],
    [workbench, "RuntimePanel", "右栏运行时面板接线"],
    [workbench, "TopBar", "顶栏合并（Q8）"],
    [workbench, "right-panel", "右栏（<1280px 抽屉）锚点"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // UI-UX §7.3 M3-03 待实现锚点必须齐备（生产代码内出现）。
  const anchors = [
    "permission-panel",
    "permission-queue-count",
    "permission-empty",
    "permission-card",
    "permission-target-raw",
    "permission-target-canonical",
    "permission-allow-once",
    "permission-allow-session",
    "permission-deny",
    "permission-timeout-note",
    "runtime-panel",
    "runtime-panel-item",
    "runtime-retry",
    "runtime-enable",
    "topbar",
    "runtime-badge",
    "storage-indicator",
    "pending-permission-badge",
  ];
  const frontend = [workbench, panel, runtimePanel, topBar].join("\n");
  for (const anchor of anchors) {
    if (!frontend.includes(anchor)) problems.push(`UI-UX 锚点缺失：${anchor}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：权限命令面/装配/锚点/契约在案", problems.length === 0 ? 0 : 1);
}

// ===== 5. 证据归档（逐用例 JSON + 汇总；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_ask_allow.json",
    "dod1_ask_deny.json",
    "dod1_timeout_deny.json",
    "dod1_once_session.json",
    "dod1_restart_pending.json",
    "dod2_queue.json",
    "dod3_disabled_runtime.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  const summary = {
    task: "M3-03",
    stamp,
    evidence_files: files,
    expect_files: expectFiles,
  };
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(summary, null, 2)}\n`,
    "utf8",
  );
  // 归档目录已位于 scripts/test/.tmp（gitignored）；此处仅打印，不复制进仓库。
  console.log(`[m3-03] 证据目录：${evidenceDir}`);
  record(
    `证据归档：7 份逐用例 JSON（ask allow/deny/timeout/once+session/restart pending/queue/disabled）`,
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-03", checks));
