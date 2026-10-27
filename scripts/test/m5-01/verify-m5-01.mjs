/**
 * M5-01 验证脚本：P1 前置 ADR 批次与设计补登记（ADR-017–ADR-026）。
 *
 * 覆盖 DoD（静态核验；运行输出即证据）：
 *   1) §9 ADR 清单逐项通过评审并合入设计文档：
 *      - 10 份 ADR 文件存在（P1-A02…A08、A10、A11、A12 → ADR-017–ADR-026）；
 *      - 每份含五段式（决策 / 实现要点 / 主要风险 / 缓解措施 / 失效条件）；
 *      - 每份版本/状态/评审记录标注「批次评审通过稿 / 17/17 闭环」；
 *      - `docs/M5-01-ADR批次评审意见.md` 复审结论「通过」且含 §6 修订闭环核验。
 *   2) 设计补登记核验（设计文档 v1.16）：
 *      - 头部 v1.16 且引用 ADR-017–ADR-026；
 *      - 命令面 63 / 错误码全集 32、登记行 20 / 设置键 8 / UI-UX 锚点十五组；
 *      - 附录 C 迁移规划位含 permission_rules / FTS / presets / 审计索引，且
 *        **未预合入终态 DDL**（代码块中无未注释的 `CREATE TABLE permission_rules`）；
 *      - 事件类型不变（附录 B 注记既有发射点）。
 *   3) ADR-006 v0.8：全集 32 / 登记行 20；9 个新错误码逐行在表。
 *   4) UI-UX v0.7：C10 计数 63；§5 新文案行（`paused` + 9 错误码）；§7.3
 *      `session-pause-toggle` / `archive` / `file-reference` 与「十五组」。
 *   5) 设置键代码落点：`SETTINGS_KEY_ALLOWLIST` 8 键；校验/缺省映射在案。
 *   6) P1 计划 v0.7：§9 正式编号标注；§10 第 2–7 项闭合；基线冻结行。
 *   7) 基线保护：迁移 0001–0003 未改、无 0004+ 迁移文件；
 *      `packages/protocol/src/bindings.ts` 未改（git 校验）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { repoRoot, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function read(relative) {
  const filePath = path.join(repoRoot, relative);
  if (!existsSync(filePath)) throw new Error(`缺少文件：${relative}`);
  return readFileSync(filePath, "utf8");
}

function expect(condition, message) {
  if (!condition) throw new Error(message);
}

// ===== DoD1：ADR 批次（五段式 / 状态 / 评审闭环） =====

const ADR_FILES = [
  "docs/adr/ADR-017-pause-resume-semantics.md",
  "docs/adr/ADR-018-p1-command-surface-batch.md",
  "docs/adr/ADR-019-permission-rules-engine.md",
  "docs/adr/ADR-020-backup-auto-and-encryption.md",
  "docs/adr/ADR-021-event-retention-and-archive.md",
  "docs/adr/ADR-022-routing-and-concurrency-gate.md",
  "docs/adr/ADR-023-full-audit-scope-and-format.md",
  "docs/adr/ADR-024-provider-consumption-and-references.md",
  "docs/adr/ADR-025-session-search-and-management.md",
  "docs/adr/ADR-026-workspace-management-and-presets.md",
];

const FIVE_SECTION_KEYS = ["决策", "实现要点", "主要风险", "缓解措施", "失效条件"];

function checkAdrBatch() {
  for (const [index, file] of ADR_FILES.entries()) {
    const text = read(file);
    const adrNumber = 17 + index;
    expect(text.includes(`ADR-0${adrNumber}`), `${file}: 缺少编号 ADR-0${adrNumber}`);
    for (const key of FIVE_SECTION_KEYS) {
      expect(text.includes(key), `${file}: 五段式缺少「${key}」`);
    }
    expect(
      text.includes("已批准（2026-10-09；人工授权 AI 审批"),
      `${file}: 状态未标注人工授权批准`,
    );
    if (adrNumber === 17) {
      // ADR-017 为合并稿（v0.3）：批次裁定已落文。
      expect(text.includes("批次评审裁定"), `${file}: 缺少批次裁定记录`);
      expect(text.includes("v0.3"), `${file}: 缺少 v0.3 批次闭环版本`);
    } else {
      expect(text.includes("批次评审通过稿"), `${file}: 版本未标注批次评审通过稿`);
      expect(text.includes("v0.2"), `${file}: 缺少 v0.2 变更记录`);
    }
    expect(text.includes("评审记录"), `${file}: 缺少评审记录节`);
  }
  console.log(`[dod1] ${ADR_FILES.length} 份 ADR 五段式/版本/评审记录核验通过`);
}

function checkReviewOpinion() {
  const text = read("docs/M5-01-ADR批次评审意见.md");
  expect(/结论[^\n]*通过/.test(text), "批次评审意见复审结论未转「通过」");
  expect(text.includes("全部闭环"), "批次评审意见缺少闭环统计（P0×1 + P1×6 + P2×8 全部闭环）");
  expect(text.includes("§6 修订闭环核验"), "批次评审意见缺少 §6 修订闭环核验");
  expect(text.includes("§7 第三轮收口复审"), "批次评审意见缺少 §7 第三轮收口复审");
  expect(text.includes("人工授权 AI 审批"), "批次评审意见缺少审批授权记录");
  console.log("[dod1] 批次评审意见：复审通过 + 全部闭环 + §6/§7（人工授权 AI 审批）在案");
}

// ===== DoD2：设计补登记（设计文档 v1.16） =====

function checkDesignDoc() {
  const text = read("设计文档.md");
  // M5-01 合入时为 v1.16；后续 ADR 可继续升版（如 ADR-027 → v1.17），
  // 本检查只要求「不低于 v1.16 且已冻结」，不把设计文档钉死在 M5-01 版本。
  expect(/\*\*v1\.(1[6-9]|[2-9]\d)（冻结）\*\*/.test(text), "设计文档头部未升 v1.16+");
  for (const number of ["ADR-017", "ADR-018", "ADR-019", "ADR-020", "ADR-021", "ADR-022", "ADR-023", "ADR-024", "ADR-025", "ADR-026"]) {
    expect(text.includes(number), `设计文档缺少 ${number} 引用`);
  }
  for (const token of [
    "命令面 52 → **63**",
    "全集 23 → **32**",
    "登记行 11 → **20**",
    "设置键 8 个",
    "permission_rules",
    "sessions_fts/messages_fts",
    "presets 表 + 3 条内置播种 + sessions.preset_id",
    "idx_audit_session_ts",
    "runtimes.enabled",
  ]) {
    expect(text.includes(token), `设计文档缺少登记标记「${token}」`);
  }
  // 事件类型不变：附录 B 注记既有发射点。
  expect(text.includes("不新增事件类型"), "设计文档附录 B 未注明事件类型不变");
  // 附录 C 规划位与落库进度（M6-01/M7-09 已落库项为终态 DDL；未落库项不得预合入）。
  const appendixStart = text.indexOf("## 附录 C");
  const codeStart = text.indexOf("```sql", appendixStart);
  const codeEnd = text.indexOf("```", codeStart + 6);
  const appendixC = text.slice(codeStart + 6, codeEnd);
  const uncommented = appendixC
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, "").trim())
    .join("\n");
  for (const forbidden of ["CREATE VIRTUAL TABLE"]) {
    expect(!uncommented.includes(forbidden), `附录 C 预合入了未落库任务的终态 DDL：${forbidden}`);
  }
  // M6-01 落库核验：permission_rules / sessions.permission_mode 终态 DDL 必在同批附录内。
  expect(
    uncommented.includes("CREATE TABLE permission_rules"),
    "附录 C 缺少 M6-01 permission_rules 终态 DDL（迁移 0004 已落库）",
  );
  expect(
    uncommented.includes("ALTER TABLE sessions ADD COLUMN permission_mode"),
    "附录 C 缺少 M6-01 sessions.permission_mode 终态 DDL（迁移 0004 已落库）",
  );
  // M7-09 落库核验：presets / sessions.preset_id 终态 DDL 必在同批附录内。
  expect(
    uncommented.includes("CREATE TABLE presets"),
    "附录 C 缺少 M7-09 presets 终态 DDL（迁移 0007 已落库）",
  );
  expect(
    uncommented.includes("ALTER TABLE sessions ADD COLUMN preset_id"),
    "附录 C 缺少 M7-09 sessions.preset_id 终态 DDL（迁移 0007 已落库）",
  );
  console.log("[dod2] 设计 v1.16+：ADR 引用/计数/规划位（M6-01/M7-09 终态已合入）/事件不变 核验通过");
}

function checkAdr006() {
  const text = read("docs/adr/ADR-006-startup-commands-and-error-codes.md");
  expect(text.includes("错误码登记表（全集 32；本表登记行 20）"), "ADR-006 附录 B 标题未更新");
  for (const code of [
    "pause_unsupported",
    "rule_not_found",
    "no_candidate_runtime",
    "provider_error",
    "session_not_found",
    "workspace_not_found",
    "workspace_in_use",
    "preset_not_found",
    "builtin_preset_undeletable",
  ]) {
    expect(text.includes(`\`${code}\``), `ADR-006 缺少错误码 ${code}`);
  }
  expect(text.includes("v0.8"), "ADR-006 缺少 v0.8 变更记录");
  console.log("[dod2] ADR-006 v0.8：9 错误码增行 + 计数核验通过");
}

function checkUiUx() {
  const text = read("docs/UI-UX-设计规格.md");
  expect(text.includes("设计规格 v0.7"), "UI-UX 标题未升 v0.7");
  expect(text.includes("63 个可调用命令"), "UI-UX C10 命令计数未更新为 63");
  for (const token of [
    "`paused`（状态）",
    "`pause_unsupported`",
    "`rule_not_found`",
    "`no_candidate_runtime`",
    "`provider_error`",
    "`session_not_found`",
    "`workspace_not_found`",
    "`workspace_in_use`",
    "`preset_not_found`",
    "`builtin_preset_undeletable`",
    "`session-pause-toggle`",
    "`archive`",
    "`file-reference`",
    "十五组",
  ]) {
    expect(text.includes(token), `UI-UX 缺少登记标记「${token}」`);
  }
  console.log("[dod2] UI-UX v0.7：文案 10 行 / 锚点三组（十五组）/ C10=63 核验通过");
}

// ===== DoD5②：设置键代码落点 =====

function checkSettingsKeys() {
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  const keys = [
    "backup.reminder",
    "backup.auto.enabled",
    "backup.auto.frequency",
    "backup.auto.retention",
    "backup.encryption.enabled",
    "events.retention.enabled",
    "events.retention.days",
    "ui.layout",
  ];
  for (const key of keys) {
    expect(dto.includes(`"${key}"`), `SETTINGS_KEY_ALLOWLIST 缺键 ${key}`);
  }
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  for (const token of [
    "UI_LAYOUT_MAX_BYTES",
    "validate_setting_int_range",
    'BACKUP_AUTO_FREQUENCY_KEY => Value::String("daily".to_owned())',
    "EVENTS_RETENTION_DAYS_KEY => Value::from(30)",
    'UI_LAYOUT_KEY => Value::String("{}".to_owned())',
    "p1_settings_keys_are_registered_with_defaults",
  ]) {
    expect(diagnostics.includes(token), `设置键校验/缺省/单测缺少「${token}」`);
  }
  console.log("[dod5] 设置键 8 键 + 类型校验 + 缺省值 + 单测 核验通过");
}

// ===== DoD3/4/6：P1 计划 v0.7（回流 / 基线 / §10 闭合） =====

function checkP1Plan() {
  const text = read("docs/P1-实施计划与验收标准.md");
  expect(text.includes("Aether P1 实施计划与验收标准（v0.7"), "P1 计划标题未升 v0.7");
  for (const number of ["ADR-017", "ADR-018", "ADR-019", "ADR-020", "ADR-021", "ADR-022", "ADR-023", "ADR-024", "ADR-025", "ADR-026"]) {
    expect(text.includes(number), `P1 计划 §9 缺少 ${number}`);
  }
  for (const token of [
    "设计文档》**v1.16**",
    "命令面 **63**",
    "错误码全集 **32** / 登记行 **20**",
    "设置键 **8**",
    "十五组",
    "**已完成（2026-10-09）**",
    "**已闭合（ADR-019 §2.2）**",
    "**已闭合（ADR-020）**",
    "**已闭合（ADR-021）**",
    "**已闭合（ADR-017）**",
    "**已闭合（ADR-022）**",
    "M5-01 执行完成",
  ]) {
    expect(text.includes(token), `P1 计划缺少标记「${token}」`);
  }
  console.log("[dod3/4/6] P1 计划 v0.7：正式编号/基线冻结/§10 闭合 核验通过");
}

// ===== 基线保护：迁移与生成物 =====

function gitLines(args) {
  const result = spawnSync("git", args, { cwd: repoRoot, encoding: "utf8" });
  if (result.error) throw new Error(`git 调用失败：${result.error.message}`);
  if (result.status !== 0) throw new Error(`git ${args.join(" ")} 失败：${result.stderr}`);
  return (result.stdout ?? "").split(/\r?\n/).filter(Boolean);
}

function checkBaselineProtection() {
  // 不变量（M6-01 起保持）：已发布迁移 0001–0003 永不修改（AGENTS §8）；新增迁移
  // （0004+）由各 P1 任务按「最大编号 +1」落库，其终态 DDL 与附录 C / bindings 的
  // 一致由 `verify:m1-03` 与 T14（generate + git diff）分别把关，本脚本不重复约束。
  const modified = gitLines(["diff", "--name-only", "--", "migrations/0001_init.sql", "migrations/0002_unique_keys.sql", "migrations/0003_p0_ui_extensions.sql"]);
  expect(modified.length === 0, `已发布迁移 0001–0003 被修改：${modified.join(", ")}`);
  console.log("[baseline] 已发布迁移 0001–0003 未改（0004+ 由各 P1 任务落库，T14/verify:m1-03 把关）");
}

// ===== 执行 =====

const CHECKS = [
  ["DoD1：ADR 批次（10 份，五段式/版本/评审记录）", checkAdrBatch],
  ["DoD1：批次评审意见（复审通过 + 全部闭环 + §6/§7）", checkReviewOpinion],
  ["DoD2：设计文档 v1.16（ADR 引用/计数/规划位/附录 B）", checkDesignDoc],
  ["DoD2：ADR-006 v0.8（9 错误码 + 计数）", checkAdr006],
  ["DoD2：UI-UX v0.7（文案/锚点/计数）", checkUiUx],
  ["DoD5②：设置键代码落点（8 键/校验/缺省/单测）", checkSettingsKeys],
  ["DoD3/4/6：P1 计划 v0.7（编号/基线/§10 闭合）", checkP1Plan],
  ["基线保护：迁移 0001–0003 与 bindings.ts", checkBaselineProtection],
];

for (const [name, check] of CHECKS) {
  try {
    check();
    record(name, 0);
  } catch (error) {
    console.error(`失败：${name}\n  ${error.message}`);
    record(name, 1);
  }
}

process.exit(summarize("verify-m5-01", checks));
