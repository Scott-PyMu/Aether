/**
 * M3-04 验证入口：备份、恢复与外部路径（设计 D13、ADR-003 决策 19、ADR-004 命令面）。
 *
 * 覆盖 DoD：
 *   1) T9 备份 → 清库 → 恢复 → 一致（行数 + 哈希抽查）——
 *      `aether-store/tests/m3_04_backup.rs`（单进程恢复引擎）+ `m3_04_backup_ipc.rs`
 *      （命令面 → 重启 → 启动序列全链）；
 *   2) 恢复七步逐项断言（`-wal`/`-shm` 改名、`.pre-restore` 保留、失败回滚）——
 *      `seven_steps_*` / `failure_after_placement_*`；
 *   3) 外部候选：高版本拒绝（提示升级）、损坏拒绝、正常成功 ——
 *      `external_candidate_validation_matrix`（store）+ `restore_rejects_*`（IPC）；
 *   4) 恢复中断（kill）→ 现场三件套完整可回退（故障注入）——
 *      `m3_04_restore_kill.rs`（子进程 `PauseAfter(ProtectScene)` → 强杀 → 启动回滚）；
 *   5) 备份到外部路径：选择器目录 + 可写 + 空间 ≥ `db+wal`×1.2、不足拒绝、产物可被
 *      恢复消费 —— `create_internal_and_external_respects_space_guard` +
 *      `external_backup_product_is_consumable_by_restore`；
 *   6) IPC 命令：`backup_list` 清单；`backup_restore` 参数/候选校验 → 恢复七步
 *      （含核心重启）；非法来源与高版本拒绝 —— 命令层 mock invoke 用例 + 后端直连。
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
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-04");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);

mkdirSync(evidenceDir, { recursive: true });

// ===== 1. aether-store：备份/恢复引擎（DoD1–4）=====

record(
  "m3_04_backup（T9 一致/七步改名与保留/失败回滚/外部候选矩阵/待处理恢复/台账）",
  run(
    cargo,
    ["test", "-p", "aether-store", "--test", "m3_04_backup", "--", "--nocapture"],
    { cwd: repoRoot },
  ),
);

record(
  "m3_04_restore_kill（DoD4：恢复中断 kill → 现场三件套完整可回退 → 启动回滚）",
  run(
    cargo,
    ["test", "-p", "aether-store", "--test", "m3_04_restore_kill", "--", "--nocapture"],
    { cwd: repoRoot },
  ),
);

// ===== 2. aether-tauri：命令面与恢复链（DoD1/3/5/6；证据归档）=====

record(
  "m3_04_backup_ipc（创建内外/空间护栏/恢复请求→启动执行→审计/候选拒绝/保留策略/命令层校验）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_04_backup_ipc", "--", "--nocapture"],
    {
      cwd: repoRoot,
      env: { AETHER_M3_04_EVIDENCE_DIR: evidenceDir },
    },
  ),
);

// ===== 3. 前端：备份/恢复页与回归（DoD5/6 UI 面）=====

record(
  "pnpm --filter @aether/desktop test（备份页清单/容量/创建/空间不足/恢复确认流 + 既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 4. T14：生成物稳定且包含 M3-04 契约（target_dir）=====

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
  const hasContract = content.includes("target_dir?: string | null");
  if (!stable) console.error("生成物与重新生成结果不一致（T14 漂移）");
  if (!hasContract) console.error("bindings.ts 缺少 BackupCreateRequest.target_dir（T14 契约缺失）");
  record("T14：重新生成稳定（幂等）且含 target_dir 契约", generated === 0 && stable && hasContract ? 0 : 1);
}

// ===== 5. 静态检查：接线与锚点 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const storeBackup = read("crates/aether-store/src/backup.rs");
  const backupControl = read("crates/aether-tauri/src/backup_control.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const dto = read("crates/aether-tauri/src/ipc/dto.rs");
  const migrate = read("crates/aether-tauri/src/startup/migrate.rs");
  const app = read("apps/desktop/src/App.tsx");
  const page = read("apps/desktop/src/BackupPage.tsx");
  const required = [
    [storeBackup, "PRE_RESTORE_INFIX", "现场保护改名标记（D13 第 4 步）"],
    [storeBackup, "RESTORE_JOURNAL_SUFFIX", "恢复现场日志（中断回滚依据）"],
    [storeBackup, "rollback_scene", "失败回滚现场"],
    [storeBackup, "recover_or_apply_pending_restore", "启动序列恢复/中断回滚入口"],
    [storeBackup, "SchemaNewerThanProgram", "高版本候选拒绝（提示升级）"],
    [backupControl, "required_space_bytes", "空间护栏 db+wal×1.2"],
    [backupControl, "backup_space_insufficient", "空间不足稳定业务码"],
    [backupControl, "backup_candidate_newer", "高版本候选稳定业务码"],
    [backupControl, "boot_apply_pending_restore", "启动序列执行七步 3–6"],
    [lib, "BackupControlBackend", "组合根接线备份命令面"],
    [lib, "boot_apply_pending_restore", "启动序列无写者窗口执行恢复"],
    [sessionBackend, "fn backup_create", "装饰器链路委派（备份创建）"],
    [sessionBackend, "fn backup_restore", "装饰器链路委派（恢复）"],
    [commands, "request_restart", "恢复请求 → 核心/应用重启（D13 第 7 步）"],
    [dto, "canonical_target_dir", "backup_create.target_dir canonicalize"],
    [migrate, "crate::disk::available_bytes", "迁移探针复用真实磁盘探针（ADR-003 #19）"],
    [app, "backup-open", "备份入口"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // UI-UX §7.3 M3-04 锚点必须齐备。
  const anchors = [
    "backup-page",
    "backup-create",
    "backup-create-label",
    "backup-list",
    "backup-item",
    "backup-restore",
    "backup-restore-external",
    "backup-restore-confirm",
    "backup-restore-result",
    "capacity-status",
    "overlay-backup",
    "overlay-back",
  ];
  const frontend = [app, page].join("\n");
  for (const anchor of anchors) {
    if (!frontend.includes(anchor)) problems.push(`UI-UX 锚点缺失：${anchor}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：接线/空间护栏/候选拒绝/锚点在案", problems.length === 0 ? 0 : 1);
}

// ===== 6. 证据归档（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod5_create_list.json",
    "dod5_space_insufficient.json",
    "dod1_t9_restore_chain.json",
    "dod3_candidate_rejections.json",
    "retention.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  const summary = {
    task: "M3-04",
    stamp,
    evidence_files: files,
    expect_files: expectFiles,
  };
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(summary, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m3-04] 证据目录：${evidenceDir}`);
  record(
    "证据归档：5 份逐用例 JSON（创建清单/空间不足/T9 恢复链/候选拒绝/保留策略）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-04", checks));
