/**
 * M5-03 验证入口：加密备份与恢复演练（设计 D13/D10/A3、ADR-020；P1 计划 §2 M5-03）。
 *
 * 覆盖 DoD：
 *   1) 加密产物检查：`AEBK1` 头与 ADR-020 附录 A-2 一致；密钥材料扫描 0 命中 ——
 *      `aether-security --lib backup_crypto`（含与 `chacha20poly1305` 参考实现
 *      逐字节对拍）+ 演练证据 `dod1_artifact_format`；
 *   2) 演练脚本（可重复、无外部凭证、禁止静默跳过）：加密备份 → 清库 → 恢复 →
 *      `quick_check` + 行数/哈希一致；加密标记与来源 id 正确 ——
 *      `scripts/test/m5-03/restore-drill.mjs`（默认 2 轮；CI/夜跑接线入口）；
 *   3) 异常路径：口令/密钥错误明确拒绝且现场三件套不被破坏；A3 降级（加密文件 +
 *      口令）行为按 ADR 断言 —— 演练证据 `dod3_*`；
 *   4) 恢复七步在加密路径逐项回归（`-wal`/`-shm`、`.pre-restore`、失败回滚）——
 *      演练证据 `dod4_scene_and_rollback`；
 *   5) 文档：恢复手册加密章节（检查）；诊断包/导出不含密钥材料（扫描 0 命中）——
 *      静态检查 + 演练证据 `dod5_secret_scan`；
 *   6) 回归：M5-02 自动备份与 M3-04 手动备份/T9 在 P1 基线上复跑（设计变更驱动：
 *      加密缺省开启，两套件夹具显式关闭加密；非放宽断言）。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp/m5-03。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);

// ===== 1. 加密底座单测（格式/流式 AEAD 对拍/主密钥/口令派生） =====

record(
  "aether-security 单测（DoD1：AEBK1 头/flags/流式 AEAD 与参考实现逐字节对拍/主密钥生成复用/口令派生/篡改拒绝）",
  run(cargo, ["test", "-p", "aether-security", "--lib", "backup_crypto"]),
);

record(
  "aether-store 单测（DoD2/4 底座：staging 写出校验 / .aebk 命名 / 恢复候选临时清理）",
  run(cargo, ["test", "-p", "aether-store", "--lib", "backup::tests"]),
);

// ===== 2. 演练（DoD2/3/4/5 集成；两轮可重复；无外部凭证；禁止静默跳过） =====

record(
  "M5-03 恢复演练（2 轮）：加密备份 → 清库 → 恢复 → quick_check+行数/哈希一致；异常路径与七步回滚",
  run(node, [path.join(repoRoot, "scripts", "test", "m5-03", "restore-drill.mjs")]),
);

// ===== 3. 回归：M5-02 自动备份 / M3-04 手动备份（T9） =====

record(
  "m5_02_auto_backup（回归：加密缺省开启后自动备份语义不回归；夹具显式关闭加密）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m5_02_auto_backup"]),
);

record(
  "m3_04_backup（回归：T9 备份→清库→恢复一致 / 七步现场保护与回滚）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_04_backup"]),
);

record(
  "m3_04_backup_ipc（回归：手动 kind=manual / 恢复链 / 命令层校验）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_04_backup_ipc"]),
);

record(
  "ipc_validation（回归：backup_create/backup_restore 参数矩阵；passphrase 增量不破严格解析）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);

// ===== 4. 前端（加密开关设置项 + 既有回归） =====

record(
  "pnpm --filter @aether/desktop test（加密开关读取/持久化 + M5-02/M3-04 备份页回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 5. T14：bindings 重新生成一致（DTO passphrase 增量） =====

record(
  "T14：bindings 重新生成一致（backup_create/backup_restore passphrase 增量）",
  run(node, [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "check"]),
);

// ===== 6. 静态检查（加密参数与 ADR 一致 / 锚点 / 文档 / 迁移基线） =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const crypto = read("crates/aether-security/src/backup_crypto.rs");
  const control = read("crates/aether-app/src/backup_control.rs");
  const auto = read("crates/aether-app/src/backup_auto.rs");
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  const pathRs = read("crates/aether-app/src/ipc/path.rs");
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const settingsTs = read("apps/desktop/src/settings.ts");
  const uiUx = read("docs/UI-UX-设计规格.md");

  const required = [
    [crypto, 'b"AEBK1"', "产物 magic（ADR-020 A-2）"],
    [crypto, "ARTIFACT_VERSION: u8 = 1", "产物版本 v1"],
    [crypto, "FLAG_PASSPHRASE_MODE: u8 = 0x01", "口令模式 flags"],
    [crypto, "FLAG_KEYCHAIN_MODE: u8 = 0x02", "Keychain 模式 flags"],
    [crypto, "SALT_LEN: usize = 16", "盐长度 16B"],
    [crypto, "NONCE_LEN: usize = 24", "nonce 24B"],
    [crypto, "TAG_LEN: usize = 16", "单一 AEAD tag 16B"],
    [crypto, "m_cost_kib: crate::encrypted_file::A3_M_COST_KIB", "Argon2id m=64MiB（A3 同参）"],
    [crypto, "t_cost: crate::encrypted_file::A3_T_COST", "Argon2id t=3"],
    [crypto, "p_cost: crate::encrypted_file::A3_P_COST", "Argon2id p=1"],
    [crypto, 'BACKUP_MASTER_KEY_SERVICE: &str = "backup"', "主密钥引用 service"],
    [crypto, 'BACKUP_MASTER_KEY_NAME: &str = "master"', "主密钥引用 key"],
    [crypto, "XChaCha20", "XChaCha20-Poly1305"],
    [control, "backup_key_unavailable", "加密不可用业务码（禁止静默降级）"],
    [control, "backup_decrypt_failed", "口令/密钥错误业务码"],
    [control, "backup_passphrase_required", "缺口令业务码"],
    [control, "RESTORE_CANDIDATE_INFIX", "解密候选临时文件（启动收尾清理）"],
    [auto, "BackupEncryption::Keychain", "自动备份加密缺省路径"],
    [auto, "backup_encrypt_failed", "加密失败业务码"],
    [dto, '"backup.encryption.enabled"', "设置键白名单（M5-01 登记）"],
    [dto, "passphrase", "backup_create/backup_restore passphrase 增量（ADR-018 A-5）"],
    [pathRs, "aebk", "外部候选 .db/.aebk 后缀"],
    [diagnostics, "BACKUP_ENCRYPTION_ENABLED_KEY", "设置键常量（缺省 true）"],
    [settingsPage, "settings-backup-encryption", "UI 锚点（加密开关）"],
    [settingsTs, "backup.encryption.enabled", "前端设置键常量"],
    [uiUx, "settings-backup-encryption", "UI-UX §7.3 实现登记"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // ADR-020 登记面。
  const adr020 = path.join(repoRoot, "docs", "adr", "ADR-020-backup-auto-and-encryption.md");
  if (!existsSync(adr020)) problems.push("缺少 docs/adr/ADR-020-backup-auto-and-encryption.md");
  else {
    const text = readFileSync(adr020, "utf8");
    for (const token of [
      "XChaCha20-Poly1305",
      "AEBK1",
      "backup.encryption.enabled",
      "keychain://aether/backup/master",
      "Argon2id",
    ]) {
      if (!text.includes(token)) problems.push(`ADR-020 缺少登记标记：${token}`);
    }
  }

  // 恢复手册（DoD5：补加密章节）。
  const manual = path.join(repoRoot, "docs", "恢复手册.md");
  if (!existsSync(manual)) problems.push("缺少 docs/恢复手册.md（恢复手册）");
  else {
    const text = readFileSync(manual, "utf8");
    for (const token of ["加密", "主密钥", "口令", "备份", "恢复", ".pre-restore"]) {
      if (!text.includes(token)) problems.push(`恢复手册缺少加密章节标记：${token}`);
    }
  }

  // 迁移基线保护（ADR-020：不改 0001–0003）。
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

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：加密参数与 ADR 一致 / 设置键与 DTO / 锚点 / 恢复手册 / 迁移基线在案",
    problems.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m5-03", checks));
