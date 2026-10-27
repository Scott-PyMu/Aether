/**
 * M5-04 验证入口：事件保留与滚动策略（设计 D4/D13、ADR-021、ADR-009）。
 *
 * 覆盖 DoD：
 *   1) 策略（时钟注入/白名单/审计保留/设置校验）—— `m5_04_retention`
 *      `cleanup_cleans_whitelist_only_and_preserves_audit` + `retention` 纯决策单测；
 *   2) 补读边界（M3-01 路径不受清理影响）—— `readback_after_cleanup_keeps_gap_contract`；
 *   3) 先归档后删除（失败跳过）—— `archive_before_delete_and_failure_skips_cleanup`
 *      + `verify-archive.mjs` 独立校验自动归档产物；
 *   4) 阈值联动（2GB 提示 / 5GB 与增长自动启停）—— `growth_threshold_auto_arms_retention`
 *      + `size_threshold_auto_arms_and_warn_level_does_not`；
 *   5) 空间回收证据与单事务口径—— `space_reclaim_evidence_recorded` +
 *      `m5_04_retention_ops`（5000 行删除事务计时 <500ms）；
 *   6) 诊断包指标（retention/export 段）—— `diagnostics_bundle_exposes_retention_and_export_metrics`。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp/m5-04。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m5-04", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. 策略单测（纯决策：窗口/增长拟合/自动启停） =====

record(
  "aether-tauri 单测（retention：cutoff/增长拟合/自动启停决策）",
  run(cargo, ["test", "-p", "aether-tauri", "--lib", "retention::tests"]),
);

// ===== 2. store 级（DoD5）：5000 行删除事务精确性与计时 =====

record(
  "m5_04_retention_ops（先归档后删除精确性 / 单事务 <500ms；证据归档）",
  run(
    cargo,
    ["test", "-p", "aether-store", "--test", "m5_04_retention_ops", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M5_04_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== 3. 集成测试（DoD1–DoD6；证据归档） =====

record(
  "m5_04_retention（DoD1 白名单/审计保留/设置校验 / DoD2 补读边界 / DoD3 归档先行与失败跳过 / DoD4 阈值联动 / DoD5 空间证据 / DoD6 诊断段）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m5_04_retention", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M5_04_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== 4. 独立校验工具（verify-archive.mjs）校验自动归档产物 =====

{
  const tool = path.join(repoRoot, "scripts", "test", "m5-05", "verify-archive.mjs");
  const archive = path.join(evidenceDir, "dod3_retention_archive.jsonl");
  record(
    "verify-archive.mjs 独立校验（自动归档：manifest/行数/sha256/白名单/0 命中）",
    existsSync(archive) ? run(node, [tool, archive]) : 1,
  );
  if (!existsSync(archive)) console.error(`独立校验输入缺失：${archive}`);
}

// ===== 5. 前端（DoD1/DoD4 UI 面 + 回归） =====

record(
  "pnpm --filter @aether/desktop test（保留设置开关/窗口/校验 + 全量回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 6. 静态接线检查 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const retention = read("crates/aether-app/src/retention.rs");
  const archiveControl = read("crates/aether-app/src/archive_control.rs");
  const storeArchive = read("crates/aether-store/src/archive.rs");
  const ops = read("crates/aether-store/src/ops.rs");
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const settingsTs = read("apps/desktop/src/settings.ts");
  const backupPage = read("apps/desktop/src/BackupPage.tsx");

  const required = [
    [storeArchive, 'RETENTION_EVENT_TYPES: [&str; 3] = ["message.delta", "usage", "log"]', "白名单（仅三类）"],
    [storeArchive, "RETENTION_BATCH_MAX", "批 ≤5000 行"],
    [ops, "DeleteEventsByIds", "rowid 精确删除（单写队列）"],
    [retention, "AUDIT_ACTION_RETENTION_CLEANED", "审计 retention.cleaned"],
    [retention, "AUDIT_ACTION_RETENTION_ARMED", "审计 retention.auto_armed"],
    [retention, "GROWTH_THRESHOLD_BYTES", "增长阈值 500MB/月"],
    [retention, "checkpoint_truncate", "空间回收（WAL 截断）"],
    [retention, "verify_archive_file", "归档产物逐批校验后才删除"],
    [archiveControl, "retention_status", "backup_list.retention / 诊断段"],
    [diagnostics, '"retention"', "诊断包 retention 段"],
    [dto, '"events.retention.enabled"', "设置键登记（白名单）"],
    [dto, '"events.retention.days"', "设置键登记（白名单）"],
    [lib, 'install_monitor("retention"', "保留任务退出序列停止"],
    [settingsPage, 'data-testid="settings-retention"', "UI-UX 锚点 settings-retention"],
    [settingsPage, 'data-testid="settings-retention-save"', "保留设置保存入口"],
    [settingsTs, "events.retention.days", "设置键常量"],
    [backupPage, 'data-testid="retention-status"', "保留状态段（备份页）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 基线保护：迁移 0001–0003 未改（ADR-021：不改迁移）。
  const diff = spawnSync(
    "git",
    [
      "diff",
      "--name-only",
      "--",
      "migrations/0001_init.sql",
      "migrations/0002_unique_keys.sql",
      "migrations/0003_p0_ui_extensions.sql",
    ],
    { cwd: repoRoot, encoding: "utf8" },
  );
  const modified = (diff.stdout ?? "").split(/\r?\n/).filter(Boolean);
  if (modified.length > 0) problems.push(`已发布迁移被修改：${modified.join(", ")}`);

  // ADR 台账：ADR-021（保留/归档）+ ADR-023（审计动作）。
  const adr021 = readFileSync(
    path.join(repoRoot, "docs", "adr", "ADR-021-event-retention-and-archive.md"),
    "utf8",
  );
  for (const token of ["auto-retention", "retention.auto_armed", "500MB"]) {
    if (!adr021.includes(token)) problems.push(`ADR-021 缺少登记标记：${token}`);
  }
  const adr023 = readFileSync(
    path.join(repoRoot, "docs", "adr", "ADR-023-full-audit-scope-and-format.md"),
    "utf8",
  );
  for (const token of ["retention.cleaned", "retention.auto_armed"]) {
    if (!adr023.includes(token)) problems.push(`ADR-023 缺少审计动作登记：${token}`);
  }

  // npm script 接线。
  const pkg = JSON.parse(read("package.json"));
  if (!pkg.scripts?.["verify:m5-04"]) problems.push("package.json 缺少脚本：verify:m5-04");

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：白名单/批量/审计/阈值/回收/设置键/UI 锚点/迁移基线/ADR 在案", problems.length === 0 ? 0 : 1);
}

// ===== 7. 证据归档 =====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir)
    : [];
  const expectFiles = [
    "dod1_cleanup_scope.json",
    "dod2_readback_boundary.json",
    "dod3_archive_before_delete.json",
    "dod3_retention_archive.jsonl",
    "dod4_growth_arm.json",
    "dod4_size_arm.json",
    "dod5_delete_transaction.json",
    "dod5_space_reclaim.json",
    "dod6_diagnostics.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify({ task: "M5-04", stamp, evidence_files: files, expect_files: expectFiles }, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m5-04] 证据目录：${evidenceDir}`);
  record(
    "证据归档：DoD1–DoD6 逐用例 JSON + 独立校验归档输入（.jsonl）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m5-04", checks));
