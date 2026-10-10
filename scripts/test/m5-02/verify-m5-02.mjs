/**
 * M5-02 验证入口：自动备份（设计 D13、ADR-020、ADR-018 附录 A-5）。
 *
 * 覆盖 DoD：
 *   1) 排期单测（时钟注入）：按 ADR 频率触发、错过窗口补备一次、频率/开关语义 ——
 *      `aether-tauri --lib backup_auto::tests`（纯决策）+ `m5_02_auto_backup`
 *      `schedule_triggers_by_frequency_with_single_catchup`（ManualClock 端到端）；
 *   2) 运行中备份正确性：备份期间持续写入 → 产物 quick_check + 行数/哈希一致 ——
 *      `backup_during_writes_is_verified_and_consistent`；写延迟不劣化于 D3 阈值
 *      （P95<50ms）—— `m5_02_backup_bench`（release + ignored，同 M1-04 口径）；
 *   3) 保留滚动与台账/磁盘一致：仅清理自动类最旧产物（手动不自动删除）——
 *      `retention_rolls_auto_only_and_table_matches_disk` + 本脚本对证据 JSON 复核；
 *   4) 失败注入（不可写/空间不足/目标不可建）：无半成品、不删既有备份、审计告警、
 *      冷却后重试、不误入 persist_degraded —— `failures_are_isolated_recorded_and_retried`
 *      + `uncreatable_target_leaves_no_half_artifact`；
 *   5) UI 面：设置项（开关/频率/保留）与最近备份状态（E2E 组件）+ 7 天提醒联动不
 *      重复告警 + 诊断包 backup_auto 段 —— 前端测试 + `reminder_linkage_and_diagnostics_section`；
 *   6) 手动备份回归：T9 通过（自动备份引入不回归）—— `m3_04_backup` /
 *      `m3_04_backup_ipc`（本脚本复跑）。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp/m5-02。
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
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m5-02", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. 排期单测与存储原子写出单测 =====

record(
  "aether-store 单测（DoD2/3 底座：临时文件+quick_check+rename 原子写出 / 按 kind 滚动 / 临时清理）",
  run(cargo, ["test", "-p", "aether-store", "--lib", "backup::tests"]),
);

record(
  "aether-tauri 单测（DoD1：排期决策 daily/weekly/补备窗口/失败冷却/关闭）",
  run(cargo, ["test", "-p", "aether-tauri", "--lib", "backup_auto::tests"]),
);

// ===== 2. 集成测试（DoD1–DoD5；证据归档） =====

record(
  "m5_02_auto_backup（DoD1 排期时钟注入 / DoD2 备份期间写入一致 / DoD3 仅自动滚动+台账磁盘一致 / DoD4 失败隔离与重试+D4 边界 / DoD5 提醒联动与诊断段）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m5_02_auto_backup", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M5_02_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== 3. DoD6：手动备份/T9 回归（P0 验收语义不变） =====

record(
  "m3_04_backup（T9 备份→清库→恢复一致 / 七步现场保护与回滚；自动备份引入不回归）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_04_backup"]),
);

record(
  "m3_04_backup_ipc（手动 kind=manual 不自动删除 / 恢复链 / 命令层校验回归）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_04_backup_ipc", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M3_04_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== 4. DoD2 基准：备份期间写事务延迟（release + ignored；D3 阈值 P95<50ms） =====

record(
  "m5_02_backup_bench（release 基准：备份期间写事务延迟 P95<50ms / 单事务<500ms）",
  run(cargo, [
    "test",
    "--release",
    "-p",
    "aether-store",
    "--test",
    "m5_02_backup_bench",
    "--",
    "--ignored",
    "--nocapture",
  ]),
);

// ===== 5. 前端（DoD5 UI 面 + 回归） =====

record(
  "pnpm --filter @aether/desktop test（设置项开关/频率/保留 E2E + 自动状态展示 + 提醒联动 + kind 标签回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 6. T14：生成物稳定（本任务无 DTO/命令变更） =====

record(
  "T14：bindings 重新生成一致（backup_list 增量不改类型面）",
  run(node, [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "check"]),
);

// ===== 7. DoD3 复核：backups 表 ↔ 磁盘清单一致（脚本对证据 JSON 再校验） =====

{
  const file = path.join(evidenceDir, "dod3_retention_consistency.json");
  let ok = false;
  let detail = "缺少证据文件";
  if (existsSync(file)) {
    const data = JSON.parse(readFileSync(file, "utf8"));
    const table = JSON.stringify([...(data.table_files ?? [])].sort());
    const disk = JSON.stringify([...(data.disk_files ?? [])].sort());
    ok = table === disk && data.auto_kept === 2 && data.manual_kept === 2;
    detail = `table=${table} disk=${disk} auto_kept=${data.auto_kept} manual_kept=${data.manual_kept}`;
  }
  if (!ok) console.error(`DoD3 台账/磁盘一致复核失败：${detail}`);
  record(
    "DoD3 复核（脚本）：backups 表 ↔ 磁盘清单逐项一致；仅自动滚动、手动保留",
    ok ? 0 : 1,
  );
}

// ===== 8. 静态接线检查 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const auto = read("crates/aether-tauri/src/backup_auto.rs");
  const control = read("crates/aether-tauri/src/backup_control.rs");
  const diagnostics = read("crates/aether-tauri/src/diagnostics_control.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const shutdown = read("crates/aether-tauri/src/shutdown.rs");
  const storeBackup = read("crates/aether-store/src/backup.rs");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const backupPage = read("apps/desktop/src/BackupPage.tsx");
  const settingsTs = read("apps/desktop/src/settings.ts");

  const required = [
    [auto, "pub fn decide", "排期决策（纯函数，时钟注入单测）"],
    [auto, "CATCHUP_WINDOW_MS", "错过窗口补备（ADR-020 §2.1-2）"],
    [auto, "RETRY_COOLDOWN_MS", "失败重试冷却（下次窗口重试）"],
    [auto, "AUDIT_ACTION_AUTO_BACKUP", "审计 backup.auto.run（ADR-023）"],
    [auto, "pub async fn execute_backup", "手动/自动共用执行路径"],
    [auto, "missed_window_catchup", "补备标记（诊断包）"],
    [auto, "next_run_at", "backup_list.auto.next_run_at（A-5）"],
    [control, "spawn_auto_backup", "组合根启动调度器"],
    [control, "auto_segment", "backup_list.auto 段"],
    [control, "backup_auto_status", "诊断包内部接线"],
    [control, "BACKUP_KIND_MANUAL", "手动产物 kind（ADR-020 §2.1-1）"],
    [storeBackup, "create_backup_file_verified", "临时文件+quick_check+rename"],
    [storeBackup, "prune_plan_for_kind", "仅自动类滚动"],
    [storeBackup, "BACKUP_TEMP_INFIX", "临时文件标记（崩溃残留清理）"],
    [diagnostics, '"backup_auto"', "诊断包 backup_auto 段（A-3）"],
    [diagnostics, "pub(crate) fn default_setting_value", "设置缺省值共享（调度器/命令同源）"],
    [lib, 'install_monitor("backup_auto"', "退出序列停止调度器"],
    [shutdown, "pub fn install_monitor", "AppShutdown 监控任务接入点"],
    [settingsPage, "settings-backup-auto-enabled", "UI-UX 锚点（开关）"],
    [settingsPage, "settings-backup-auto-frequency", "UI-UX 锚点（频率）"],
    [settingsPage, "settings-backup-auto-retention", "UI-UX 锚点（保留）"],
    [settingsPage, "settings-backup-auto-save", "设置保存入口"],
    [backupPage, "backup-auto-status", "备份页自动状态可查"],
    [backupPage, "kindLabel", "manual/auto 标签"],
    [settingsTs, "backup.auto.enabled", "设置键常量"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 基线保护：迁移 0001–0003 未改（ADR-020：不改迁移）。
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

  // ADR 台账：ADR-020（自动备份与加密）与 ADR-018 附录 A-5（auto 段字段冻结）。
  const adr020 = path.join(repoRoot, "docs", "adr", "ADR-020-backup-auto-and-encryption.md");
  if (!existsSync(adr020)) problems.push("缺少 docs/adr/ADR-020-backup-auto-and-encryption.md");
  else {
    const text = readFileSync(adr020, "utf8");
    for (const token of ["consecutive_failures", "backup.auto.retention", "backup.auto.enabled"]) {
      if (!text.includes(token)) problems.push(`ADR-020 缺少登记标记：${token}`);
    }
  }
  const adr018 = readFileSync(
    path.join(repoRoot, "docs", "adr", "ADR-018-p1-command-surface-batch.md"),
    "utf8",
  );
  for (const token of ["next_run_at", "consecutive_failures", "backup.auto.retention"]) {
    if (!adr018.includes(token)) problems.push(`ADR-018 附录 A-5 缺少字段登记：${token}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：调度器/组合根/诊断段/设置键/前端锚点/迁移基线在案", problems.length === 0 ? 0 : 1);
}

// ===== 9. 证据归档 =====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_schedule.json",
    "dod2_backup_during_writes.json",
    "dod3_retention_consistency.json",
    "dod4_failure_isolation.json",
    "dod4_io_failure.json",
    "dod5_reminder_linkage.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify({ task: "M5-02", stamp, evidence_files: files, expect_files: expectFiles }, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m5-02] 证据目录：${evidenceDir}`);
  record(
    "证据归档：6 份逐用例 JSON（排期/写入一致/滚动一致/失败隔离/IO 失败/提醒联动）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m5-02", checks));
