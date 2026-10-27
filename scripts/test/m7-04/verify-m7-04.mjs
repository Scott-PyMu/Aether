#!/usr/bin/env node
/**
 * M7-04 验证脚本：标签页与分屏（UI-01 完整；设计 §2.3「多视图」P1、D8、§4#20）。
 *
 * 覆盖：
 * - 静态：布局模型落点（`ui.layout` = ADR-018 附录 A-6 设置键，无迁移）、
 *   `layout.ts` 解析/损坏回退/读写、`WorkbenchTabs`/`SplitPane` 锚点与
 *   EventStore 按会话订阅、UI-UX v0.8 登记（C1/§7.3 锚点/§8#1）、计划进度；
 * - DoD1：`m7_04_tabs_split`（创建/切换/关闭/分屏 + 各视图 EventStore 隔离）；
 * - DoD2：`m7_04_perf`（10 会话 × 50 delta/s：帧预算 / 事件延迟 P95 / 批处理）
 *   + P0 基准回归（`eventStore.integration` 10×50 口径、`workbenchScroll` 帧预算）；
 * - DoD3：`layout.test`（settings `ui.layout` 读写/损坏回退/引用归位）
 *   + `m7_04_tabs_split`（重启恢复断言）；
 * - DoD4：`m7_04_tabs_split`（角标 data-status/data-group 与 `sessionGroupOf` 一致）
 *   + `m3_12_session_groups`（分组口径回归）；
 * - DoD5：UI-UX 规格 v0.8 静态登记（锚点/冻结注/C1 修订）+ 前端 typecheck；
 * - 回归：`SessionWorkbench`、@aether/desktop 全量；Rust 设置键注册/校验与
 *   命令面（`p1_settings*` / `m3_05_diagnostics` / `ipc_validation`）；
 *   `verify:m1-03`（未改迁移）。
 *
 * 用法：node scripts/test/m7-04/verify-m7-04.mjs
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-04", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function vitest(files) {
  return run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
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

  // 1) 布局模型：settings 键落点（ADR-018 附录 A-6）+ 纯函数 + 损坏回退。
  const layoutTs = read("apps/desktop/src/layout.ts");
  for (const token of [
    "LAYOUT_VERSION = 1",
    "MAX_TABS = 32",
    "export interface WorkbenchLayout",
    "export function defaultLayout",
    "export function parseLayoutValue",
    "export async function loadLayout",
    "export async function saveLayout",
    "export function openTab",
    "export function activateTab",
    "export function closeTab",
    "export function setSplit",
    "export function clampLayout",
    "UI_LAYOUT_KEY",
  ]) {
    assert(layoutTs.includes(token), `layout.ts 缺少「${token}」`);
  }
  assert(
    !layoutTs.includes("@tauri-apps"),
    "layout.ts 不得直连 Tauri（应经 settings IPC 注入）",
  );
  const settingsTs = read("apps/desktop/src/settings.ts");
  assert(
    settingsTs.includes('export const UI_LAYOUT_KEY = "ui.layout";'),
    "settings.ts 缺少 ui.layout 键常量",
  );

  // 2) 后端设置键登记（M5-01 DoD5②/ADR-018 附录 A-6）：白名单 + 类型校验 + 缺省。
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  assert(dto.includes('"ui.layout"'), "SETTINGS_KEY_ALLOWLIST 缺 ui.layout");
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  for (const token of [
    'pub const UI_LAYOUT_KEY: &str = "ui.layout";',
    "UI_LAYOUT_MAX_BYTES",
    "UI_LAYOUT_KEY => {",
    "default_setting_value",
  ]) {
    assert(diagnostics.includes(token), `diagnostics_control.rs 缺少「${token}」`);
  }

  // 3) 标签栏/分屏组件锚点 + 隔离订阅 + 分组口径同源。
  const tabs = read("apps/desktop/src/WorkbenchTabs.tsx");
  for (const token of [
    'data-testid="workbench-tabs"',
    'data-testid="workbench-tabs-empty"',
    'data-testid="session-tab"',
    'data-testid="session-tab-title"',
    'data-testid="session-tab-badge"',
    'data-testid="session-tab-close"',
    'data-testid="split-toggle"',
    "data-status={group}",
    "data-background={String(background)}",
    'data-group={group}',
    "sessionGroupOf",
  ]) {
    assert(tabs.includes(token), `WorkbenchTabs 缺少「${token}」`);
  }
  const split = read("apps/desktop/src/SplitPane.tsx");
  for (const token of [
    'data-testid="split-pane"',
    'data-testid="split-message-stream"',
    'testId="split-message-list"',
    'data-testid="split-message-bubble"',
    'data-testid="split-composer-input"',
    'data-testid="split-composer-send"',
    "useSessionEvents(store, sessionId)",
  ]) {
    assert(split.includes(token), `SplitPane 缺少「${token}」`);
  }

  // 4) 工作台接线：布局恢复/持久化、标签/分屏渲染、状态角标刷新事件。
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of [
    "loadLayout(",
    "saveLayout(",
    "openTab(",
    "activateTab(",
    "closeTab(",
    "setSplit(",
    "clampLayout(",
    "<WorkbenchTabs",
    "<SplitPane",
    'data-testid="pane-primary"',
    "session.status_changed",
    "settingsIpc: settingsIpcProp = productionSettingsIpc",
  ]) {
    assert(workbench.includes(token), `SessionWorkbench 缺少「${token}」`);
  }

  // 5) UI-UX 规格 v0.8：C1 修订 + 锚点登记并冻结 + §8#1 更新（DoD5）。
  const uiux = read("docs/UI-UX-设计规格.md");
  for (const token of [
    "# Aether UI/UX 设计规格 v0.8",
    "v0.8 为 M7-04 标签页/分屏锚点与 C1 修订登记",
    "P1 起标签页/分屏在单窗口内开放（M7-04）",
    "`workbench-tabs`",
    "`session-tab`",
    "`session-tab-badge`",
    "`split-toggle`",
    "`split-pane`",
    "`pane-primary`",
    "M7-04 锚点契约（2026-10-18 任务登记并冻结）",
    "P1 已实现（M7-04）：标签页 / 双窗格分屏",
    "ui.layout",
  ]) {
    assert(uiux.includes(token), `UI-UX 规格缺少「${token}」`);
  }

  // 6) 计划进度与验证入口登记。
  const plan = read("docs/P1-实施计划与验收标准.md");
  assert(plan.includes("#### M7-04 标签页与分屏"), "P1 计划缺 M7-04 任务节");
  assert(plan.includes("执行状态：✅ 已完成（2026-10-18）"), "P1 计划缺 M7-04 执行状态");
  const rootPackage = JSON.parse(read("package.json"));
  assert(
    rootPackage.scripts?.["verify:m7-04"] ===
      "node scripts/test/m7-04/verify-m7-04.mjs",
    "package.json 缺 verify:m7-04 脚本",
  );
  assert(
    existsSync(path.join(repoRoot, "docs/M7-04-证据.md")),
    "docs/M7-04-证据.md 缺失",
  );

  record("静态：布局落点/组件锚点/接线/UI-UX v0.8/计划登记逐项", 0);
} catch (error) {
  record(`静态检查失败：${error && error.message ? error.message : error}`, 1);
}

// ===== DoD3（模型层）：布局解析/损坏回退/读写失败路径/纯函数 =====
record("DoD3 前端 layout（21 用例：解析/回退/持久化/纯函数）", vitest(["src/layout.test.ts"]));

// ===== DoD1 + DoD3 + DoD4（组件集成）：标签/分屏/隔离/重启恢复/角标 =====
record(
  "DoD1/3/4 前端 m7_04_tabs_split（8 用例：交互/隔离/恢复/回退/角标）",
  vitest(["src/m7_04_tabs_split.test.tsx"]),
);

// ===== DoD2：10 会话 × 50 delta/s 基准（帧预算 / 事件延迟 / 批处理） =====
record(
  "DoD2 前端 m7_04_perf（10 会话 × 50 delta/s 基准）",
  vitest(["src/m7_04_perf.test.tsx"]),
);

// ===== DoD2 回归：P0 基准口径（10×50 批处理；10k 消息帧预算） =====
record(
  "回归 P0 基准 eventStore.integration（M3-01：10 会话 × 50 delta/s）",
  vitest(["src/eventStore.integration.test.tsx"]),
);
record(
  "回归 P0 基准 workbenchScroll（M3-02：10k 消息帧预算）",
  vitest(["src/workbenchScroll.test.tsx"]),
);

// ===== DoD4 回归：M3-12 分组口径 =====
record(
  "回归 M3-12 分组 m3_12_session_groups（2 用例）",
  vitest(["src/m3_12_session_groups.test.tsx"]),
);

// ===== 回归：P0 工作台与全量前端套件 =====
record("回归 前端 SessionWorkbench（5 用例）", vitest(["src/SessionWorkbench.test.tsx"]));
record(
  "回归 @aether/desktop 全量（M7-04 后 227 用例）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "test"]),
);

// ===== DoD5（编译）：前端 typecheck =====
record(
  "DoD5 前端 typecheck（tsc --noEmit）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);

// ===== DoD3（后端）：ui.layout 登记键缺省/类型校验 =====
record(
  "DoD3 aether-tauri p1_settings（登记键缺省/类型校验）",
  run(cargo, ["test", "-p", "aether-tauri", "--lib", "p1_settings"]),
);
record(
  "回归 aether-tauri m3_05_diagnostics settings_validation_and_persistence",
  run(cargo, [
    "test",
    "-p",
    "aether-tauri",
    "--test",
    "m3_05_diagnostics",
    "settings_validation_and_persistence",
  ]),
);
record(
  "回归 aether-tauri ipc_validation（命令面/参数校验矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);

// ===== 回归：迁移 ↔ 附录 C 双向比对（M7-04 无迁移） =====
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 证据归档 =====
const summary = {
  task: "M7-04",
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
console.log(`[m7-04] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-04", checks));
