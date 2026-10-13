/**
 * M6-01 验证脚本：用户自定义权限规则与 `always` 作用域 + 会话级权限模式三档
 * （设计 D9 修订 / ADR-014 附录 B、ADR-019；P1 计划 M6-01 DoD1–DoD11）。
 *
 * 覆盖：
 *  - DoD1：迁移 0004（`sessions.permission_mode` + `permission_rules`）表/约束/索引、
 *    增量迁移幂等与历史行不受影响（store 集成 + `verify:m1-03` 双向比对）；
 *  - DoD2/3/5/6/8：三档矩阵逐项、`always` 持久化与重启重建、规则层决策顺序
 *    （deny 优先 / 硬边界不可放宽 / 规则变更不影响 pending）、命令侧快照刷新；
 *  - DoD4：规则五命令全链路 + 非法规则拒绝（tauri 集成）；
 *  - DoD5/DoD6：回环零直通与等待态回归（m2-10 / m3-03 / m3-12 / m3-08 复跑）；
 *  - DoD7/DoD8⑨：UI 锚点与常驻边界声明（静态检查 + vitest）；
 *  - DoD9：IPC 命令矩阵与生成物（ipc_validation + bindings 静态核验）。
 */
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

// ===== 静态：附录 C 终态 DDL / 迁移文件 / 生成物 / UI 锚点 =====

function checkStatic() {
  const doc = read("设计文档.md");
  const appendixStart = doc.indexOf("## 附录 C");
  const codeStart = doc.indexOf("```sql", appendixStart);
  const codeEnd = doc.indexOf("```", codeStart + 6);
  const appendixC = doc
    .slice(codeStart + 6, codeEnd)
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, "").trim())
    .join("\n");
  if (!appendixC.includes("CREATE TABLE permission_rules")) {
    throw new Error("附录 C 缺少 permission_rules 终态 DDL");
  }
  if (!appendixC.includes("ALTER TABLE sessions ADD COLUMN permission_mode")) {
    throw new Error("附录 C 缺少 sessions.permission_mode 终态 DDL");
  }

  const migration = read("migrations/0004_permission_mode_and_rules.sql");
  if (!migration.includes("permission_mode")) {
    throw new Error("迁移 0004 缺少 permission_mode");
  }
  if (!migration.includes("CREATE TABLE permission_rules")) {
    throw new Error("迁移 0004 缺少 permission_rules");
  }
  const embedded = read("crates/aether-store/src/migration.rs");
  if (!embedded.includes("0004_permission_mode_and_rules.sql")) {
    throw new Error("EMBEDDED_MIGRATIONS 未内嵌 0004");
  }

  // 生成物（T14 口径：内容由 tauri-specta 生成；此处静态核验关键契约存在）。
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const token of [
    "__TAURI_INVOKE(\"session_permission_mode_set\"",
    "__TAURI_INVOKE(\"rules_list\"",
    "__TAURI_INVOKE(\"rule_add\"",
    "__TAURI_INVOKE(\"rule_update\"",
    "__TAURI_INVOKE(\"rule_delete\"",
    "__TAURI_INVOKE(\"rule_toggle\"",
    "\"rule_not_found\"",
    'export type PermissionDecision = "once" | "session" | "always" | "deny"',
    "permission_mode?: string | null",
  ]) {
    if (!bindings.includes(token)) {
      throw new Error(`bindings.ts 缺少契约 token：${token}`);
    }
  }

  // UI 锚点（M6-01 DoD8⑨ / ADR-014 附录 F-1；实施不得重命名）。
  const selector = read("apps/desktop/src/PermissionModeSelector.tsx");
  for (const token of [
    "permission-mode-selector",
    "permission-mode-item",
    "permission-mode-boundary",
    "danger_full_access",
  ]) {
    if (!selector.includes(token)) {
      throw new Error(`PermissionModeSelector 缺少锚点/档位 token：${token}`);
    }
  }
  if (!selector.includes("档位由核心权限门与本机运行时沙箱共同执行")) {
    throw new Error("常驻边界声明文案缺失（ADR-014 附录 F-3）");
  }
  const panel = read("apps/desktop/src/PermissionPanel.tsx");
  if (!panel.includes("permission-allow-always")) {
    throw new Error("审批弹窗缺少「记住」入口（permission-allow-always）");
  }
  const rulesPage = read("apps/desktop/src/RulesPage.tsx");
  for (const token of ["rules-page", "rule-item", "rule-delete-confirm-button"]) {
    if (!rulesPage.includes(token)) {
      throw new Error(`RulesPage 缺少锚点 token：${token}`);
    }
  }
  const settings = read("apps/desktop/src/SettingsPage.tsx");
  if (!settings.includes("settings-tab-rules")) {
    throw new Error("设置页缺少规则管理 tab（settings-tab-rules）");
  }
  console.log("[static] 附录 C 终态 DDL / 迁移 0004 / bindings 契约 / UI 锚点 核验通过");
}

// ===== 执行 =====

try {
  checkStatic();
  record("静态：附录 C 终态 DDL / 迁移 0004 / bindings / UI 锚点", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：附录 C 终态 DDL / 迁移 0004 / bindings / UI 锚点", 1);
}

record(
  "DoD1：迁移 0004 表/约束/索引 + 增量迁移幂等（store 集成）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m6_01_permission_schema"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD1：附录 C ↔ 迁移集 ↔ 真实库双向比对（verify:m1-03）",
  run(process.execPath, ["scripts/test/m1-03/verify-m1-03.mjs"], { cwd: repoRoot }),
);
record(
  "DoD2/5/6：P0 矩阵回归（m2-03，矩阵口径按 ADR-014 更新）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_03_permission"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD2/3/5/6/8：三档矩阵/always 持久化/规则层（control 集成）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m6_01_permission_modes"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD6：等待态回归（m3-12）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m3_12_waiting_state"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD4：规则五命令 + rule_not_found（tauri 集成）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m6_01_rules"], { cwd: repoRoot }),
);
record(
  "DoD5/6：回环零直通 + 策略直决回归（m2-10）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m2_10_permission_loop"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD4/5：权限中心命令面回归（m3-03）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_03_permission_center"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD8⑦：记忆写入档位回归（m3-08）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_08_memory"], { cwd: repoRoot }),
);
record(
  "DoD4/9：IPC 参数校验矩阵（含新命令；ipc_validation）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);
record(
  "DoD2/3：策略引擎与规则模型单测（aether-security）",
  run(cargo, ["test", "-p", "aether-security", "permission::"], { cwd: repoRoot }),
);
record(
  "DoD8⑨：UI 选择器/规则页/「记住」入口（vitest 定向）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m6_01_permission_modes.test.tsx",
    "src/m6_01_rules.test.tsx",
  ]),
);
record(
  "DoD9：前端类型检查（tsc --noEmit）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);

process.exit(summarize("verify-m6-01", checks));
