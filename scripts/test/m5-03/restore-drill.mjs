/**
 * M5-03 加密备份恢复演练（ADR-020 附录 A-4 契约；设计 D13/D10/A3）。
 *
 * 演练链路（可重复执行；CI/夜跑接线入口）：
 *   加密备份 → 清库 → 恢复 → `quick_check` + 行数/哈希抽查一致；
 *   加密标记与来源 id 正确；口令/密钥错误路径与 A3 降级路径断言。
 *
 * 约束（ADR-020 A-4）：
 *   - **不依赖外部凭证**：主密钥存储使用测试内确定性实现 / A3 加密文件（口令模式）；
 *   - **禁止静默跳过**：缺 Cargo/测试目标即失败（非跳过），任一步非零退出即失败；
 *   - 默认连续执行 2 轮（`--runs N` 可调），每轮独立证据目录并逐项复核 JSON。
 *
 * 用法：node scripts/test/m5-03/restore-drill.mjs [--runs 2]
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const runsIndex = process.argv.indexOf("--runs");
const runs = runsIndex >= 0 ? Number(process.argv[runsIndex + 1] ?? 2) : 2;
if (!Number.isInteger(runs) || runs < 1) {
  console.error(`[m5-03] 非法 --runs：${process.argv[runsIndex + 1]}`);
  process.exit(2);
}

const cargo = bin("cargo");
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const stamp = new Date().toISOString().replace(/[:.]/g, "-");

/** 逐项复核单轮证据 JSON（返回问题清单）。 */
function validateEvidence(evidenceDir) {
  const problems = [];
  const read = (name) => {
    const file = path.join(evidenceDir, `${name}.json`);
    if (!existsSync(file)) {
      problems.push(`缺少证据文件：${name}.json`);
      return null;
    }
    try {
      return JSON.parse(readFileSync(file, "utf8"));
    } catch (error) {
      problems.push(`证据 JSON 解析失败：${name}.json（${error.message}）`);
      return null;
    }
  };
  const expectTrue = (value, label) => {
    if (value !== true) problems.push(`${label}：期望 true，实际 ${JSON.stringify(value)}`);
  };
  const expectContains = (value, needle, label) => {
    if (typeof value !== "string" || !value.includes(needle)) {
      problems.push(`${label}：未包含 ${needle}（实际 ${JSON.stringify(value)}）`);
    }
  };

  // DoD1：格式与密钥材料扫描。
  const dod1 = read("dod1_artifact_format");
  if (dod1) {
    if (dod1.keychain?.magic !== "AEBK1") problems.push("DoD1：keychain magic 非 AEBK1");
    if (dod1.keychain?.version !== 1) problems.push("DoD1：keychain version 非 1");
    if (dod1.keychain?.flags !== 2) problems.push("DoD1：keychain flags 非 0x02");
    if (dod1.passphrase?.flags !== 1) problems.push("DoD1：passphrase flags 非 0x01");
    if ((dod1.keychain?.secret_scan_hits ?? ["?"]).length !== 0) {
      problems.push("DoD1：keychain 产物密钥材料扫描非 0 命中");
    }
    if ((dod1.passphrase?.secret_scan_hits ?? ["?"]).length !== 0) {
      problems.push("DoD1：passphrase 产物密钥材料扫描非 0 命中");
    }
    expectTrue(dod1.scan_positive_control, "DoD1 阳性对照");
  }

  // DoD2：两模式演练（七步/一致/加密标记/来源 id）。
  for (const name of ["dod2_drill_keychain", "dod2_drill_passphrase"]) {
    const drill = read(name);
    if (!drill) continue;
    const steps = drill.steps ?? [];
    const expected = [
      "validate_candidate",
      "stop_writes",
      "protect_scene",
      "place_candidate",
      "verify",
    ];
    if (JSON.stringify(steps) !== JSON.stringify(expected)) {
      problems.push(`${name}：恢复七步序列不符（${JSON.stringify(steps)}）`);
    }
    if (drill.fingerprint_before !== drill.fingerprint_after) {
      problems.push(`${name}：恢复前后哈希不一致`);
    }
    if (drill.sessions_before !== drill.sessions_after) {
      problems.push(`${name}：恢复前后行数不一致`);
    }
    expectTrue(drill.quick_check_ok, `${name} quick_check`);
    expectTrue(drill.artifact_encrypted, `${name} 加密标记`);
    if (drill.source_backup_id !== drill.backup_id) {
      problems.push(`${name}：来源 id 与台账 id 不一致`);
    }
    expectTrue(drill.candidate_response?.encrypted, `${name} 候选加密标记`);
  }

  // DoD3：拒绝路径 / A3 降级 / 自动备份暂停。
  const reject = read("dod3_reject_paths");
  if (reject) {
    expectContains(reject.missing_passphrase, "backup_passphrase_required", "DoD3 缺口令");
    expectContains(reject.wrong_passphrase, "backup_decrypt_failed", "DoD3 错口令");
    expectContains(reject.key_unavailable, "backup_key_unavailable", "DoD3 主密钥不可用");
    expectTrue(reject.journal_absent, "DoD3 拒绝路径无 journal");
    if ((reject.candidate_residue ?? ["?"]).length !== 0) {
      problems.push("DoD3：存在解密候选残留");
    }
    if (JSON.stringify(reject.scene_before) !== JSON.stringify(reject.scene_after)) {
      problems.push("DoD3：现场三件套被改动");
    }
  }
  const a3 = read("dod3_a3_degraded");
  if (a3) {
    expectTrue(a3.artifact_encrypted, "DoD3 A3 加密标记");
    expectTrue(a3.wrong_a3_passphrase_rejected, "DoD3 A3 口令错误拒绝");
    expectTrue(a3.restore_candidate?.encrypted, "DoD3 A3 恢复候选加密");
  }
  const auto = read("dod3_auto_pause_and_explicit_disable");
  if (auto) {
    expectContains(auto.failed_error, "backup_key_unavailable", "DoD3 自动暂停业务码");
    if (auto.storage_state !== "normal") problems.push("DoD3：自动暂停误入存储降级");
    if (auto.recovered !== "success") problems.push("DoD3：显式关闭加密后自动备份未恢复");
    expectTrue(auto.recovered_encrypted === false, "DoD3 显式关闭加密 → 明文产物");
  }

  // DoD4：加密路径现场保护与回滚。
  const dod4 = read("dod4_scene_and_rollback");
  if (dod4) {
    expectContains(dod4.rollback_error, "已回滚现场", "DoD4 回滚");
    if (dod4.rollback_pre_restore_files !== 0) problems.push("DoD4：回滚后 .pre-restore 残留");
    if (dod4.pre_restore_files !== 3) problems.push("DoD4：现场三件套保护数非 3");
    expectTrue(dod4.journal_finalized, "DoD4 journal 收尾");
  }

  // DoD5：密钥材料扫描。
  const dod5 = read("dod5_secret_scan");
  if (dod5) {
    for (const field of ["bundle_hits", "audit_hits", "response_hits"]) {
      if ((dod5[field] ?? ["?"]).length !== 0) {
        problems.push(`DoD5：${field} 非 0 命中（${JSON.stringify(dod5[field])}）`);
      }
    }
    expectTrue(dod5.scan_positive_control, "DoD5 阳性对照");
  }
  return problems;
}

for (let round = 1; round <= runs; round += 1) {
  const evidenceDir = path.join(
    repoRoot,
    "scripts",
    "test",
    ".tmp",
    "m5-03",
    `drill-${stamp}-r${round}`,
  );
  mkdirSync(evidenceDir, { recursive: true });
  const exit = run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m5_03_encrypted_backup", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M5_03_EVIDENCE_DIR: evidenceDir } },
  );
  record(`演练第 ${round}/${runs} 轮（加密备份 → 清库 → 恢复 → 一致；无外部凭证）`, exit);

  const problems = exit === 0 ? validateEvidence(evidenceDir) : ["演练执行失败，跳过证据复核"];
  if (problems.length > 0) console.error(problems.join("\n"));
  const files = readdirSync(evidenceDir).filter((name) => name.endsWith(".json"));
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify({ task: "M5-03", drill: true, round, runs, files }, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m5-03] 演练第 ${round} 轮证据目录：${evidenceDir}`);
  record(`演练第 ${round} 轮证据复核（七步/一致/加密标记/来源 id/扫描 0 命中）`, problems.length === 0 ? 0 : 1);
}

process.exit(summarize("m5-03-restore-drill", checks));
