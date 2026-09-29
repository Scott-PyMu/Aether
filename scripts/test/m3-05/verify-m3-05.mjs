/**
 * M3-05 验证入口：诊断与容量巡检（设计 D11/D13、ADR-007 增量、ADR-003 决策 19）。
 *
 * 覆盖 DoD：
 *   1) 诊断包密钥模式扫描 0 命中 —— `m3_05_diagnostics.rs::export_bundle_*`
 *      （`sk-`/`eyJ`/PEM 三类样本 → 脱敏 → 再扫描；`scanned_clean=true` 守门）；
 *   2) 容量阈值参数化模拟 → 警告/强提示横幅正确 —— `capacity_thresholds_*`
 *      （注入阈值 → `backup_list.capacity.level` 三档）+ 前端容量横幅测试；
 *   3) 7 天未备份提醒可开关（时钟注入）—— `backup_reminder_*`（`ManualClock` +
 *      `settings` 持久化）+ 前端提醒/开关测试；
 *   4) 日志汇聚与诊断包整合 —— 同 DoD1 用例断言 `attempt=n/3`、`persist_degraded`
 *      进入诊断包，脱敏扫描 0 命中；降级启动态导出可用（D4）。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp。
 */
import { createHash } from "node:crypto";
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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-05");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

mkdirSync(evidenceDir, { recursive: true });

// ===== 1. aether-store：设置读写与库摘要（DoD1/DoD3 的存储支撑）=====

record(
  "aether-store --lib（设置 upsert/读回、库摘要表清单齐备、备份/迁移回归）",
  run(cargo, ["test", "-p", "aether-store", "--lib"], { cwd: repoRoot }),
);

// ===== 2. aether-tauri：诊断后端（DoD1–4；证据归档）=====

record(
  "m3_05_diagnostics（脱敏 0 命中/日志汇聚整合/容量参数化/提醒时钟注入/空间护栏/命令层外部路径）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_05_diagnostics", "--", "--nocapture"],
    {
      cwd: repoRoot,
      env: { AETHER_M3_05_EVIDENCE_DIR: evidenceDir },
    },
  ),
);

record(
  "ipc_validation（M1-08 校验矩阵回归：导出目标外部路径语义/设置键白名单）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);

record(
  "m2_07_logging（日志汇聚端回归：attempt=n/3 与 persist_degraded 可导出源）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m2_07_logging"], { cwd: repoRoot }),
);

// ===== 3. 前端：诊断页/设置页/关于页/右栏入口/备份提醒（DoD2/3 的 UI 面）=====

record(
  "pnpm --filter @aether/desktop test（诊断导出/容量横幅/提醒开关/锚点与既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 4. T14：生成物稳定（命令面未新增；导出目标语义为附加校验）=====

{
  const target = path.join(repoRoot, "packages", "protocol", "src", "bindings.ts");
  const hash = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");
  const before = existsSync(target) ? hash(target) : null;
  const generated = run(node, [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "generate"], {
    cwd: repoRoot,
  });
  const after = existsSync(target) ? hash(target) : null;
  const content = existsSync(target) ? readFileSync(target, "utf8") : "";
  const stable = before !== null && before === after;
  if (!stable) console.error("生成物与重新生成结果不一致（T14 漂移）");
  const hasContract = content.includes("export_diagnostics") && content.includes("settings_set");
  if (!hasContract) console.error("bindings.ts 缺少诊断/设置命令契约（T14）");
  record("T14：重新生成稳定（幂等）且含诊断/设置契约", generated === 0 && stable && hasContract ? 0 : 1);
}

// ===== 5. 静态检查：接线/脱敏/阈值参数化/锚点 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const diagnostics = read("crates/aether-tauri/src/diagnostics_control.rs");
  const coreHealth = read("crates/aether-tauri/src/core_health.rs");
  const backupControl = read("crates/aether-tauri/src/backup_control.rs");
  const securityLevel = read("crates/aether-tauri/src/security_level.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const dto = read("crates/aether-tauri/src/ipc/dto.rs");
  const storeOps = read("crates/aether-store/src/ops.rs");
  const app = read("apps/desktop/src/App.tsx");
  const diagnosticsPage = read("apps/desktop/src/DiagnosticsPage.tsx");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const aboutPage = read("apps/desktop/src/AboutPage.tsx");
  const entry = read("apps/desktop/src/DiagnosticsEntry.tsx");
  const backupPage = read("apps/desktop/src/BackupPage.tsx");
  const healthMonitor = read("apps/desktop/src/HealthMonitor.tsx");
  const sessionWorkbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const required = [
    [diagnostics, "Redactor", "诊断包脱敏器（D10）"],
    [diagnostics, "redact_json", "递归脱敏（日志/库摘要/配置）"],
    [diagnostics, "is_clean", "脱敏后 0 命中守门"],
    [diagnostics, "scanned_clean", "导出回执守门结果"],
    [diagnostics, "BACKUP_REMINDER_THRESHOLD_MS", "7 天未备份提醒阈值（D13）"],
    [diagnostics, "diagnostics_space_insufficient", "导出空间护栏稳定业务码"],
    [diagnostics, "TaskDumpSource", "任务 dump 整合源（M2-05 缓冲）"],
    [diagnostics, "diagnostics_target_not_writable", "导出目标不可写稳定业务码"],
    [backupControl, "CapacityConfig", "容量阈值参数化（M3-05 DoD2）"],
    [backupControl, "CAPACITY_WARN_ENV", "阈值环境变量覆盖（模拟）"],
    [coreHealth, "degraded_provider", "降级健康快照（诊断包健康段）"],
    [securityLevel, "self_check", "A3 凭据库自检探针"],
    [lib, "DiagnosticsControlBackend", "组合根接线诊断后端"],
    [lib, "security_level::probe", "启动安全级别探针接线"],
    [commands, "canonical_target_dir", "导出目标 canonicalize（外部路径语义）"],
    [dto, "backup.reminder", "设置键白名单登记"],
    [storeOps, "UpsertSetting", "设置写入经单写队列"],
    [storeOps, "store_summary", "库摘要（诊断包）"],
    [healthMonitor, "storage-degraded-diagnostics", "降级横幅诊断入口（M3-06 承接关闭）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // UI-UX §7.3 M3-05 锚点必须齐备（含实现登记行）。
  const anchors = [
    "diagnostics-page",
    "diagnostics-target",
    "diagnostics-pick",
    "diagnostics-export",
    "diagnostics-result",
    "diagnostics-capacity",
    "diagnostics-error",
    "settings-page",
    "settings-data-dir",
    "settings-security-level",
    "settings-backup-reminder",
    "settings-workspace",
    "about-page",
    "about-version",
    "about-protocol",
    "about-security-boundary",
    "about-beta-marker",
    "settings-open",
    "diagnostics-open",
    "about-open",
    "right-panel-diagnostics",
    "diagnostics-entry-capacity",
    "diagnostics-entry-open",
    "backup-reminder",
    "backup-reminder-dismiss",
    "overlay-settings",
    "overlay-diagnostics",
    "overlay-about",
    "overlay-back",
  ];
  const frontend = [
    app,
    diagnosticsPage,
    settingsPage,
    aboutPage,
    entry,
    backupPage,
    healthMonitor,
    sessionWorkbench,
  ].join("\n");
  for (const anchor of anchors) {
    if (!frontend.includes(anchor)) problems.push(`UI-UX 锚点缺失：${anchor}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：脱敏/阈值参数化/接线/锚点在案", problems.length === 0 ? 0 : 1);
}

// ===== 6. 证据归档（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_dod4_bundle.json",
    "dod4_degraded_export.json",
    "dod4_tracing_aggregation.json",
    "dod2_capacity_levels.json",
    "dod3_reminder.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  const summary = {
    task: "M3-05",
    stamp,
    evidence_files: files,
    expect_files: expectFiles,
  };
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(summary, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m3-05] 证据目录：${evidenceDir}`);
  record(
    "证据归档：5 份逐用例 JSON（脱敏+日志整合/tracing 汇聚/降级导出/容量三档/提醒）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-05", checks));
