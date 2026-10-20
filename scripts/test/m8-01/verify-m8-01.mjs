#!/usr/bin/env node
/**
 * M8-01 验证入口：P1 故障注入与夜跑（P1 计划 §5 M8-01 DoD1–DoD3；ADR-012/ADR-013）。
 *
 * 分层口径（沿用 P0 M4-01/02/03 先例）：任务级测试（M5–M7）验证单点行为正确；
 * 本任务验证 P1 代码基线上「端到端脚本化执行稳定 + 三段证据归档」，编排既有权威
 * 测试并提取其证据行/产物，**不重复实现断言、不放宽任何断言**。
 *
 * DoD1：P1 新机制故障场景（9 项，每场景「触发 → 自动应对 → 恢复」三段证据）：
 *   1) encrypted-backup-corrupt-passphrase  加密备份损坏/口令错误（m5_03）
 *   2) retention-interrupted-archive-failure 保留清理中断/归档失败（m5_04 + ops）
 *   3) rules-bypass-attempts                规则绕过尝试（m6_01 + m6_02）
 *   4) hotplug-midflight-crash              热插拔中途崩溃（m7_01_hotplug）
 *   5) pause-deadlock                       暂停死锁（m7_03_pause control + tauri）
 *   6) routing-no-candidate                 路由无候选（m7_02_routing）
 *   7) audit-export-interruption            审计/导出中断（m6_03_audit）
 *   8) provider-connectivity-timeout-auth   连通性测试超时/凭据失效（m7_06）
 *   9) reference-injection-path-exception   引用注入路径异常（ipc_validation + m3_09 + mock）
 *
 * DoD2：P0 30 场景回归（M4-01 11 + M4-02 8 + M4-03 11）在 P1 基线全量重跑，
 *       场景 id 与 P0 基线逐一比对；不退化（差异须记录理由并经评审）。
 *
 * DoD3：连续 ≥3 轮夜跑全绿（预注册；同一执行期连续执行，不要求跨自然日）。
 *       一轮 = DoD1 全量 + DoD2 全量；flake 逐条记录与处置（flake-ledger.json）。
 *
 * 运行：
 *   node scripts/test/m8-01/verify-m8-01.mjs --rounds 3      # 预注册夜跑（门禁口径）
 *   node scripts/test/m8-01/verify-m8-01.mjs --rounds 1      # 本地开发单轮
 *   node scripts/test/m8-01/verify-m8-01.mjs --only pause-deadlock   # 开发定位（非门禁）
 *   参数 --skip-p0：跳过 DoD2（仅开发定位；门禁/夜跑禁止使用，脚本会标记不完整）。
 *
 * 证据归档：scripts/test/.tmp/m8-01/run-<stamp>/round-<n>/（场景 JSON/日志/测试证据/
 * P0 回归摘要）+ run-<stamp>/summary.json（轮次汇总 + flake 台账）。
 */
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run } from "../lib/exec.mjs";
import { createDrill, parseOnly, stampNow } from "../m4/lib/drill.mjs";

const cargo = bin("cargo");
const node = process.execPath;
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);
const fixtureEnv = { AETHER_FIXTURE_BIN: fixture, AETHER_REQUIRE_FIXTURE: "1" };
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();

// 夜跑期间禁止 cargo 访问网络/索引锁（依赖已锁定；降低并行构建争用）。
process.env.CARGO_NET_OFFLINE ??= "true";

function argValue(flag, fallback) {
  const index = process.argv.indexOf(flag);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  return value && !value.startsWith("--") ? value : fallback;
}

const roundsRequired = Number(argValue("--rounds", "1"));
const skipP0 = process.argv.includes("--skip-p0");
const only = parseOnly();
const runStamp = stampNow();
const runRoot = path.join(repoRoot, "scripts", "test", ".tmp", "m8-01", `run-${runStamp}`);

if (!Number.isInteger(roundsRequired) || roundsRequired < 1) {
  console.error(`[m8-01] --rounds 非法：${argValue("--rounds", "")}`);
  process.exit(2);
}
mkdirSync(runRoot, { recursive: true });

// ===== DoD1 场景清单（缺一不可） =====

const DOD1_SCENARIOS = [
  "encrypted-backup-corrupt-passphrase",
  "retention-interrupted-archive-failure",
  "rules-bypass-attempts",
  "hotplug-midflight-crash",
  "pause-deadlock",
  "routing-no-candidate",
  "audit-export-interruption",
  "provider-connectivity-timeout-auth",
  "reference-injection-path-exception",
];

// ===== DoD2 P0 30 场景基线（M4-01 11 + M4-02 8 + M4-03 11） =====

const P0_BASELINE = [
  {
    task: "m4-01",
    title: "进程类失败场景（11）",
    ids: [
      "task-panic",
      "core-oom",
      "exit-hang",
      "ui-lag",
      "start-crash",
      "untrusted-adapter",
      "runtime-crash",
      "deaf-hang",
      "orphan",
      "output-flood",
      "double-open-adapter",
    ],
  },
  {
    task: "m4-02",
    title: "存储类失败场景（8）",
    ids: [
      "disk-full",
      "db-corruption",
      "wal-growth",
      "write-backlog",
      "double-open-write",
      "persist-failure",
      "sequencer-crash",
      "readback-gap",
    ],
  },
  {
    task: "m4-03",
    title: "协议与并发类失败场景（11）",
    ids: [
      "half-line",
      "invalid-json",
      "request-timeout",
      "version-mismatch",
      "stdout-log",
      "slow-ui",
      "outbound-backlog",
      "cancel-storm",
      "deadlock-task",
      "broadcast-full",
      "delivery-backlog",
    ],
  },
];
const P0_TOTAL = P0_BASELINE.reduce((sum, suite) => sum + suite.ids.length, 0);

// ===== 静态前置检查（预注册 / 夜跑接线 / 包脚本） =====

function preflight() {
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  try {
    const preReg = read("docs/M8-01-夜跑预注册.md");
    for (const token of ["--rounds 3", "连续 3 轮", "flake", "encrypted-backup-corrupt-passphrase"]) {
      if (!preReg.includes(token)) problems.push(`预注册文档缺少标记：${token}`);
    }
  } catch {
    problems.push("缺少 docs/M8-01-夜跑预注册.md（夜跑方法预注册，先于执行冻结）");
  }
  try {
    const workflow = read(".github/workflows/m8-nightly.yml");
    if (!workflow.includes("verify-m8-01.mjs --rounds 3")) {
      problems.push("m8-nightly.yml 未以 --rounds 3 调用 verify-m8-01.mjs");
    }
    if (!workflow.includes("scripts/test/.tmp/m8-01/")) {
      problems.push("m8-nightly.yml 未归档 m8-01 证据目录");
    }
  } catch {
    problems.push("缺少 .github/workflows/m8-nightly.yml（夜跑接线）");
  }
  try {
    const pkg = JSON.parse(read("package.json"));
    if (!pkg.scripts?.["verify:m8-01"]) problems.push("package.json 缺少 verify:m8-01");
  } catch (error) {
    problems.push(`package.json 不可解析：${error.message}`);
  }
  for (const suite of P0_BASELINE) {
    if (!existsSync(path.join(repoRoot, "scripts", "test", suite.task, `verify-${suite.task}.mjs`))) {
      problems.push(`缺少 P0 演练脚本：scripts/test/${suite.task}/verify-${suite.task}.mjs`);
    }
  }
  return problems;
}

// ===== 前置构建（夹具：热插拔 / 暂停场景） =====

function buildFixture() {
  console.log("\n$ cargo build -p aether-adapters --bin aether-adapter-fixture   # M8-01 夹具");
  const exit = run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]);
  if (exit !== 0 || !existsSync(fixture)) {
    console.error(`[m8-01] 夹具构建失败（exit=${exit}）：${fixture}`);
    return false;
  }
  return true;
}

// ===== 场景标记辅助 =====

/** libtest 用例通过行（--nocapture 下仍输出；作为该阶段断言通过的证据行）。 */
function okMarker(phase, testName) {
  return { phase, prefix: `test ${testName} ... ok` };
}

/** 测试内 write_evidence 行（`[task] 证据 name = {json}`）按字段断言。 */
function evidenceMarker(phase, prefix, assert) {
  return { phase, prefix, assert };
}

/** 构造 cargo 测试步骤（--nocapture 以保留证据行）。 */
function testStep({ name, pkg, testBin, filter, markers, env, retries = 0, timeoutMs = 30 * 60 * 1000 }) {
  return {
    name,
    command: cargo,
    args: [
      "test",
      "-p",
      pkg,
      "--test",
      testBin,
      ...(filter ? [filter] : []),
      "--",
      "--nocapture",
    ],
    env,
    retries,
    timeoutMs,
    markers,
  };
}

/** vitest 步骤（verbose reporter 输出用例行，作为证据）。 */
function vitestStep({ name, packageName, file, testName, markers, retries = 0 }) {
  return {
    name,
    command: pnpm,
    args: [
      ...pnpmPrefix,
      "--filter",
      packageName,
      "exec",
      "vitest",
      "run",
      "--reporter=verbose",
      file,
      ...(testName ? ["-t", testName] : []),
    ],
    retries,
    timeoutMs: 10 * 60 * 1000,
    markers,
  };
}

// ===== DoD1 场景定义 =====

function defineScenarios(drill, scenariosDir) {
  const backupDir = path.join(scenariosDir, "m5-03");
  const retentionDir = path.join(scenariosDir, "m5-04");
  const auditDir = path.join(scenariosDir, "m6-03");
  const hotplugDir = path.join(scenariosDir, "m7-01");

  // 1) 加密备份损坏 / 口令错误 / 主密钥不可用 → 拒绝且现场不被破坏 → 正确口令恢复。
  drill.define({
    id: "encrypted-backup-corrupt-passphrase",
    name: "加密备份损坏/口令错误",
    source: "DoD1 / D13 / D10 / ADR-020（M5-03）",
    trigger: "加密产物被篡改（flags 位翻转）；恢复请求缺口令/错口令；主密钥存储不可用（A3 降级边界）",
    response:
      "明确拒绝（backup_passphrase_required / backup_decrypt_failed / backup_key_unavailable / backup_candidate_invalid），现场 db+wal 哈希不变、journal 不登记、无解密候选残留",
    recovery: "正确口令走恢复演练：解密候选 → 七步 2–6 → quick_check + 会话行数/指纹一致，候选随收尾清理",
    steps: [
      testStep({
        name: "m5_03 DoD1 加密产物形态（AEBK1/flags/salt）与密钥材料扫描 0 命中",
        pkg: "aether-tauri",
        testBin: "m5_03_encrypted_backup",
        filter: "dod1_artifact_format_matches_adr_and_secret_scan_is_clean",
        env: { AETHER_M5_03_EVIDENCE_DIR: backupDir },
        markers: [
          evidenceMarker(
            "trigger",
            "[m5-03] 证据 dod1_artifact_format =",
            (value) =>
              value.keychain?.magic === "AEBK1" &&
              value.keychain?.flags === 2 &&
              value.passphrase?.flags === 1 &&
              value.passphrase?.salt_len === 16 &&
              value.keychain?.secret_scan_hits?.length === 0,
          ),
        ],
      }),
      testStep({
        name: "m5_03 DoD3 损坏/错口令/主密钥不可用 → 明确拒绝且现场三件套不被破坏",
        pkg: "aether-tauri",
        testBin: "m5_03_encrypted_backup",
        filter: "dod3_wrong_credentials_reject_without_touching_scene",
        env: { AETHER_M5_03_EVIDENCE_DIR: backupDir },
        markers: [
          evidenceMarker(
            "trigger",
            "[m5-03] 证据 dod3_reject_paths =",
            (value) =>
              String(value.missing_passphrase).includes("backup_passphrase_required") &&
              String(value.wrong_passphrase).includes("backup_decrypt_failed") &&
              String(value.key_unavailable).includes("backup_key_unavailable") &&
              (String(value.tampered_artifact).includes("backup_candidate_invalid") ||
                String(value.tampered_artifact).includes("backup_decrypt_failed")),
          ),
          evidenceMarker(
            "response",
            "[m5-03] 证据 dod3_reject_paths =",
            (value) =>
              JSON.stringify(value.scene_before) === JSON.stringify(value.scene_after) &&
              value.journal_absent === true &&
              Array.isArray(value.candidate_residue) &&
              value.candidate_residue.length === 0,
          ),
        ],
      }),
      testStep({
        name: "m5_03 DoD2 口令模式恢复演练（quick_check + 行数/指纹一致）",
        pkg: "aether-tauri",
        testBin: "m5_03_encrypted_backup",
        filter: "dod2_restore_drill_passphrase_mode",
        env: { AETHER_M5_03_EVIDENCE_DIR: backupDir },
        markers: [
          evidenceMarker(
            "recovery",
            "[m5-03] 证据 dod2_drill_passphrase =",
            (value) =>
              value.quick_check_ok === true &&
              value.fingerprint_before === value.fingerprint_after &&
              value.sessions_before === value.sessions_after &&
              value.restart_required === true,
          ),
        ],
      }),
    ],
  });

  // 2) 保留清理中断 / 归档失败（归档失败跳过清理；删除单事务精确、无部分删除）。
  drill.define({
    id: "retention-interrupted-archive-failure",
    name: "保留清理中断/归档失败",
    source: "DoD1 / D13 / D3 / ADR-021（M5-04）",
    trigger: "归档目录被同名文件占位 → 归档创建失败；清理批删除故障注入（单事务中断面）",
    response:
      "归档失败 → 跳过该批清理（零行删除）+ archive_failures_total 计数 + retention.cleaned(result=failed) 审计；删除走单事务精确行集，不产生部分删除",
    recovery: "解除注入 → 先归档（manifest 行数/哈希校验通过）后删除；批量删除 <500ms；空间回收记录与 db+wal 实测差值一致",
    steps: [
      testStep({
        name: "m5_04 DoD3 归档失败 → 跳过清理；解除后先归档后删除且产物可校验",
        pkg: "aether-tauri",
        testBin: "m5_04_retention",
        filter: "archive_before_delete_and_failure_skips_cleanup",
        env: { AETHER_M5_04_EVIDENCE_DIR: retentionDir },
        markers: [
          evidenceMarker(
            "trigger",
            "[m5-04] 证据 dod3_archive_before_delete =",
            (value) => value.failed_injection === "failed",
          ),
          evidenceMarker(
            "response",
            "[m5-04] 证据 dod3_archive_before_delete =",
            (value) =>
              Number(value.retention?.archive_failures_total) >= 1 &&
              value.retention?.last_result === "success" &&
              value.archive_rows === 3,
          ),
          evidenceMarker(
            "recovery",
            "[m5-04] 证据 dod3_archive_before_delete =",
            (value) =>
              value.retention?.last_result === "success" &&
              value.retention?.last_cleaned_rows === 3 &&
              value.archive_rows === 3,
          ),
        ],
      }),
      testStep({
        name: "m5_04-ops DoD5 5000 行批删除精确性 + 单事务 <500ms（无部分删除）",
        pkg: "aether-store",
        testBin: "m5_04_retention_ops",
        filter: "retention_batch_delete_is_exact_and_bounded",
        env: { AETHER_M5_04_EVIDENCE_DIR: retentionDir },
        markers: [
          evidenceMarker(
            "response",
            "[m5-04-ops] 证据 dod5_delete_transaction =",
            (value) =>
              value.deleted_rows === 5000 &&
              value.remaining_total === 1000 &&
              value.remaining_control === 500 &&
              value.remaining_in_window === 500 &&
              value.archive_matches === true,
          ),
          evidenceMarker(
            "recovery",
            "[m5-04-ops] 证据 dod5_delete_transaction =",
            (value) => value.hard_threshold_met === true,
          ),
        ],
      }),
      testStep({
        name: "m5_04 DoD5 空间回收：db+wal 前后值 + checkpoint 截断 + 600 行清理",
        pkg: "aether-tauri",
        testBin: "m5_04_retention",
        filter: "space_reclaim_evidence_recorded",
        env: { AETHER_M5_04_EVIDENCE_DIR: retentionDir },
        markers: [
          evidenceMarker(
            "recovery",
            "[m5-04] 证据 dod5_space_reclaim =",
            (value) =>
              Number(value.freed_bytes_recorded) > 0 &&
              value.retention?.last_cleaned_rows === 600 &&
              value.retention?.last_result === "success",
          ),
        ],
      }),
    ],
  });

  // 3) 规则绕过尝试：硬边界不可放宽、T7 逃逸样本、deny 优先、resolver 审计。
  drill.define({
    id: "rules-bypass-attempts",
    name: "规则绕过尝试",
    source: "DoD1 / D9 / ADR-019 / ADR-014 附录 B（M6-01/M6-02）",
    trigger:
      "规则 allow 尝试放宽硬边界（exec/net、区外路径）与 T7 逃逸样本（..、软链接/Junction、UNC/扩展前缀、8.3 短名、ADS、尾随点/空格、保留设备名）；always 授权降级重求值",
    response:
      "规则只改变决议来源：deny 收紧、allow 不越过档位/硬边界；逃逸样本不命中；pending 票据不被追溯改写；零直通（100% 经网关 + 审计 resolver=rule:<id>）",
    recovery: "拒绝后合法规则仍按预期放行/收紧（deny 规则立即收紧且审计可查），会话继续可用",
    steps: [
      testStep({
        name: "m6_02 DoD3 T7 逃逸样本/零直通/硬边界安全回归",
        pkg: "aether-control",
        testBin: "m6_02_rule_safety",
        filter: "m6_02_case_analysis_data_integrity_and_safety_regression",
        markers: [
          okMarker(
            "trigger",
            "m6_02_case_analysis_data_integrity_and_safety_regression",
          ),
        ],
      }),
      testStep({
        name: "m6_01 DoD8 规则 allow 解析 ask 且不能放宽硬边界",
        pkg: "aether-control",
        testBin: "m6_01_permission_modes",
        filter: "rule_allow_resolves_ask_and_cannot_widen_hard_boundary",
        markers: [
          okMarker("response", "rule_allow_resolves_ask_and_cannot_widen_hard_boundary"),
        ],
      }),
      testStep({
        name: "m6_01 DoD8 deny 规则收紧 + resolver=rule:<id> 审计",
        pkg: "aether-control",
        testBin: "m6_01_permission_modes",
        filter: "rule_deny_tightens_allow_and_audits_resolver",
        markers: [
          okMarker("response", "rule_deny_tightens_allow_and_audits_resolver"),
        ],
      }),
      testStep({
        name: "m6_01 DoD8 always 授权持久化 + 降级重求值（重启重建）",
        pkg: "aether-control",
        testBin: "m6_01_permission_modes",
        filter: "always_grant_persists_and_downgrade_reevaluates",
        markers: [
          okMarker("recovery", "always_grant_persists_and_downgrade_reevaluates"),
        ],
      }),
    ],
  });

  // 4) 热插拔中途崩溃：在途 run 期间禁用/终止 → 收口 + 隔离 + 重启用重放；10 次循环无孤儿。
  drill.define({
    id: "hotplug-midflight-crash",
    name: "热插拔中途崩溃",
    source: "DoD1 / D5 / ADR-015（M7-01）",
    trigger:
      "在途 run（long）与排队 run 存在时禁用运行时（热插拔中途终止：进程树回收 / 连接断连），另一官方运行时保持 Ready",
    response:
      "活跃 run 收口 adapter_disconnected/cli_exit、queued run adapter_unavailable（均 recoverable）；隔离断言其他运行时不受影响；10 次循环无孤儿进程/台账残留",
    recovery: "重启用后 run_retry 重放成功；连续 10 次热插拔循环 RSS/句柄增量受控（脚本化阈值）",
    steps: [
      testStep({
        name: "m7_01 DoD2 在途确认门 → 收口/隔离 → 重启用重放",
        pkg: "aether-tauri",
        testBin: "m7_01_hotplug",
        filter: "disable_confirmation_closes_in_flight_runs_and_retry_replays",
        env: { ...fixtureEnv, AETHER_M7_01_EVIDENCE_DIR: hotplugDir },
        timeoutMs: 30 * 60 * 1000,
        retries: 1,
        markers: [
          okMarker("trigger", "disable_confirmation_closes_in_flight_runs_and_retry_replays"),
          okMarker("response", "disable_confirmation_closes_in_flight_runs_and_retry_replays"),
          okMarker("recovery", "disable_confirmation_closes_in_flight_runs_and_retry_replays"),
        ],
      }),
      testStep({
        name: "m7_01 DoD4 连续 10 次热插拔循环：无孤儿/台账不残留/增长受控",
        pkg: "aether-tauri",
        testBin: "m7_01_hotplug",
        filter: "ten_hotplug_cycles_leave_no_orphans_and_bounded_growth",
        env: { ...fixtureEnv, AETHER_M7_01_EVIDENCE_DIR: hotplugDir },
        timeoutMs: 30 * 60 * 1000,
        retries: 1,
        markers: [
          okMarker("trigger", "ten_hotplug_cycles_leave_no_orphans_and_bounded_growth"),
          okMarker("response", "ten_hotplug_cycles_leave_no_orphans_and_bounded_growth"),
          okMarker("recovery", "ten_hotplug_cycles_leave_no_orphans_and_bounded_growth"),
        ],
      }),
    ],
  });

  // 5) 暂停死锁：受控中断、等待槽保留、best-effort、看门狗不适用、恢复重放。
  drill.define({
    id: "pause-deadlock",
    name: "暂停死锁",
    source: "DoD1 / D6 / D5 / ADR-017（M7-03）",
    trigger:
      "在途 run 执行中暂停（受控中断）；`session.pause` 下发失败注入；waiting_permission 状态暂停；paused 期间看门狗/断流超时输入",
    response:
      "受控中断收口 run.cancelled(user_pause) + session.status_changed(paused)；下发失败 best-effort 不阻塞收口；等待槽保留不提升不取消；看门狗对 paused 不适用（无死锁，用例在超时内完成）",
    recovery: "resume 重放原输入（Mode R；复用原输入消息不新增消息行）；重启后 paused 保留并可 resume→idle；seq 连续无缺口",
    steps: [
      testStep({
        name: "m7_03 DoD1/2/6 在途 run 受控中断 → paused → resume 重放（审计/统计/seq 连续）",
        pkg: "aether-control",
        testBin: "m7_03_pause",
        filter: "pause_running_session_interrupts_and_resume_replays",
        timeoutMs: 30 * 60 * 1000,
        markers: [
          okMarker("trigger", "pause_running_session_interrupts_and_resume_replays"),
          okMarker("response", "pause_running_session_interrupts_and_resume_replays"),
          okMarker("recovery", "pause_running_session_interrupts_and_resume_replays"),
        ],
      }),
      testStep({
        name: "m7_03 DoD3 下发失败不阻塞暂停收口（best-effort）",
        pkg: "aether-control",
        testBin: "m7_03_pause",
        filter: "pause_notify_failure_does_not_block_pause",
        markers: [okMarker("response", "pause_notify_failure_does_not_block_pause")],
      }),
      testStep({
        name: "m7_03 DoD1/2 暂停保留等待队列（不提升、不取消）",
        pkg: "aether-control",
        testBin: "m7_03_pause",
        filter: "pause_keeps_waiting_run_and_resume_precedes_it",
        markers: [okMarker("response", "pause_keeps_waiting_run_and_resume_precedes_it")],
      }),
      testStep({
        name: "m7_03 DoD4 看门狗/断流超时对 paused 不适用",
        pkg: "aether-control",
        testBin: "m7_03_pause",
        filter: "watchdog_does_not_touch_paused_session",
        markers: [okMarker("response", "watchdog_does_not_touch_paused_session")],
      }),
      testStep({
        name: "m7_03 DoD4 重启后 paused 保留 → resume 返回 idle",
        pkg: "aether-control",
        testBin: "m7_03_pause",
        filter: "paused_survives_restart_and_resume_returns_idle",
        markers: [okMarker("recovery", "paused_survives_restart_and_resume_returns_idle")],
      }),
      testStep({
        name: "m7_03 夹具端到端：session.pause 收口 + 恢复重放（无死锁）",
        pkg: "aether-tauri",
        testBin: "m7_03_pause",
        filter: "pause_interrupts_and_resume_replays_through_fixture",
        env: fixtureEnv,
        timeoutMs: 30 * 60 * 1000,
        retries: 1,
        markers: [
          okMarker("trigger", "pause_interrupts_and_resume_replays_through_fixture"),
          okMarker("recovery", "pause_interrupts_and_resume_replays_through_fixture"),
        ],
      }),
    ],
  });

  // 6) 路由无候选：全部禁用 / 无匹配模型 / 无 ready 候选 → 结构化错误；恢复后路由成功。
  drill.define({
    id: "routing-no-candidate",
    name: "路由无候选",
    source: "DoD1 / CP-02 / ADR-022（M7-02）",
    trigger:
      "自动路由无候选：全部禁用/无匹配模型（无启用 provider 提供该模型）/监督器 disabled 状态排除；热插拔禁用即排除（无粘性缓存）",
    response: "结构化错误 no_candidate_runtime（三类原因）+ 零会话创建/零下发；手动指定 enabled=0 → invalid_value；已禁用运行时不参与打分",
    recovery: "启用 provider + 模型行/重启用运行时 → 按固定优先级选 claude-code，写 route.decided 审计 + session.created.routed=true",
    steps: [
      testStep({
        name: "m7_02 DoD1 模型硬条件：无启用 provider → no_candidate_runtime；补齐后路由成功",
        pkg: "aether-tauri",
        testBin: "m7_02_routing",
        filter: "model_condition_requires_enabled_provider",
        markers: [
          okMarker("trigger", "model_condition_requires_enabled_provider"),
          okMarker("response", "model_condition_requires_enabled_provider"),
          okMarker("recovery", "model_condition_requires_enabled_provider"),
        ],
      }),
      testStep({
        name: "m7_02 DoD2 热插拔联动：禁用即排除、启用即重新纳入",
        pkg: "aether-tauri",
        testBin: "m7_02_routing",
        filter: "hotplug_disable_and_enable_toggles_routing_candidates",
        markers: [okMarker("response", "hotplug_disable_and_enable_toggles_routing_candidates")],
      }),
      testStep({
        name: "m7_02 DoD2 监督器 disabled 状态排除（即使偏好被改写）",
        pkg: "aether-tauri",
        testBin: "m7_02_routing",
        filter: "supervisor_disabled_status_excluded_from_auto_route",
        markers: [
          okMarker("response", "supervisor_disabled_status_excluded_from_auto_route"),
        ],
      }),
      testStep({
        name: "m7_02 DoD1/5 固定优先级路由 + route.decided 审计 + routed 回显",
        pkg: "aether-tauri",
        testBin: "m7_02_routing",
        filter: "auto_route_uses_fixed_priority_and_audits_decision",
        markers: [okMarker("recovery", "auto_route_uses_fixed_priority_and_audits_decision")],
      }),
    ],
  });

  // 7) 审计/导出中断：失败注入不留半成品；外部路径护栏；成功导出可校验。
  drill.define({
    id: "audit-export-interruption",
    name: "审计/导出中断",
    source: "DoD1 / SE-03 / ADR-023（M6-03）",
    trigger: "审计导出脱敏阶段失败注入（逐行 sanitizer 返回 Err）；外部目标不可写/空间不足",
    response:
      "导出失败向上传播且不留半成品/暂存残留（leftovers=0）；外部护栏 archive_target_not_writable / archive_space_insufficient，失败路径不新增产物",
    recovery: "正常导出：manifest 行数/哈希校验通过（verified=true）、脱敏 0 命中、过滤导出行数正确",
    steps: [
      testStep({
        name: "m6_03 DoD3 导出失败注入 → 无半成品/无暂存残留",
        pkg: "aether-tauri",
        testBin: "m6_03_audit",
        filter: "audit_export_failure_leaves_no_partial_artifact",
        env: { AETHER_M6_03_EVIDENCE_DIR: auditDir },
        markers: [
          evidenceMarker(
            "trigger",
            "[m6-03] 证据 dod3_interrupt =",
            (value) => value.no_partial_artifact === true,
          ),
          evidenceMarker(
            "response",
            "[m6-03] 证据 dod3_interrupt =",
            (value) => value.leftovers === 0,
          ),
        ],
      }),
      testStep({
        name: "m6_03 DoD3 外部目标护栏（不可写/空间不足，失败不新增产物）",
        pkg: "aether-tauri",
        testBin: "m6_03_audit",
        filter: "audit_export_external_target_guards",
        env: { AETHER_M6_03_EVIDENCE_DIR: auditDir },
        markers: [
          evidenceMarker(
            "response",
            "[m6-03] 证据 dod3_external_guards =",
            (value) =>
              value.not_writable_code === "archive_target_not_writable" &&
              value.insufficient_code === "archive_space_insufficient",
          ),
        ],
      }),
      testStep({
        name: "m6_03 DoD3 正常导出可校验（manifest 行数/哈希/脱敏/过滤）",
        pkg: "aether-tauri",
        testBin: "m6_03_audit",
        filter: "audit_export_is_verifiable_filtered_and_redacted",
        env: { AETHER_M6_03_EVIDENCE_DIR: auditDir },
        markers: [
          evidenceMarker(
            "recovery",
            "[m6-03] 证据 dod3_export =",
            (value) =>
              value.verified === true &&
              value.redacted === true &&
              value.rows === 3 &&
              value.filtered_rows === 2,
          ),
        ],
      }),
    ],
  });

  // 8) 供应商连通性测试超时/凭据失效 → 结构化错误；修复后成功。
  drill.define({
    id: "provider-connectivity-timeout-auth",
    name: "供应商连通性测试超时/凭据失效",
    source: "DoD1 / D10 / D7 / ADR-024（M7-06）",
    trigger: "本地 HTTP 桩：401/403（凭据失效）；接受连接但不响应（>1.5s 总超时）；缺 api_key_ref",
    response:
      "provider_auth_failed（含状态码）/ provider_unreachable（含「超时」）/ invalid_value(api_key_ref) 且零请求；响应不含密钥本体",
    recovery: "200 正常端点：ok=true、status=200、models_count 归一化去重、latency_ms 记录，密钥不漏出",
    steps: [
      testStep({
        name: "m7_06 超时 → provider_unreachable（含「超时」）",
        pkg: "aether-tauri",
        testBin: "m7_06_provider_connectivity",
        filter: "timeout_maps_to_provider_unreachable_with_timeout_text",
        markers: [okMarker("trigger", "timeout_maps_to_provider_unreachable_with_timeout_text")],
      }),
      testStep({
        name: "m7_06 凭据失效（401/403）→ provider_auth_failed（含状态码）",
        pkg: "aether-tauri",
        testBin: "m7_06_provider_connectivity",
        filter: "auth_failure_maps_to_provider_auth_failed",
        markers: [
          okMarker("trigger", "auth_failure_maps_to_provider_auth_failed"),
          okMarker("response", "auth_failure_maps_to_provider_auth_failed"),
        ],
      }),
      testStep({
        name: "m7_06 缺 api_key_ref → invalid_value 且零请求",
        pkg: "aether-tauri",
        testBin: "m7_06_provider_connectivity",
        filter: "missing_api_key_ref_returns_invalid_value_without_request",
        markers: [
          okMarker("response", "missing_api_key_ref_returns_invalid_value_without_request"),
        ],
      }),
      testStep({
        name: "m7_06 成功路径：ok/latency/models_count + 无密钥泄漏",
        pkg: "aether-tauri",
        testBin: "m7_06_provider_connectivity",
        filter: "success_path_returns_ok_latency_and_models_count",
        markers: [okMarker("recovery", "success_path_returns_ok_latency_and_models_count")],
      }),
    ],
  });

  // 9) 引用注入路径异常：畸形矩阵 / 不可用路径 → 拒绝；合法引用注入被适配器观测。
  drill.define({
    id: "reference-injection-path-exception",
    name: "引用注入路径异常",
    source: "DoD1 / UI-07 / D6 / ADR-024（M7-06）",
    trigger:
      "session.send.references 畸形矩阵：>32 条、kind 非 file/directory、未知字段、相对路径；artifact_add 不可用路径（不存在/目录当文件/特殊形态）",
    response:
      "invalid_value（字段 references / references[].kind）且不触达后端；artifact_add → artifact_path_rejected；不产生事件、不预授权（读取仍走 fs.read 权限门）",
    recovery: "合法引用（canonicalize + 可访问性通过）到达后端并原样注入适配器（Mock session-log has_references=true/path/kind），畸形样本零直通",
    steps: [
      testStep({
        name: "M1-08 references 畸形矩阵（条数/类型/未知字段/相对路径）+ 不触达后端",
        pkg: "aether-tauri",
        testBin: "ipc_validation",
        filter: "malformed_samples_return_structured_errors_and_do_not_reach_backend",
        markers: [
          okMarker(
            "trigger",
            "malformed_samples_return_structured_errors_and_do_not_reach_backend",
          ),
          okMarker(
            "response",
            "malformed_samples_return_structured_errors_and_do_not_reach_backend",
          ),
        ],
      }),
      testStep({
        name: "M3-09 artifact_add 不可用路径 → artifact_path_rejected（不落库）",
        pkg: "aether-tauri",
        testBin: "m3_09_artifacts",
        filter: "dod2_artifact_add_rejects_unusable_paths_with_dedicated_code",
        markers: [
          okMarker("response", "dod2_artifact_add_rejects_unusable_paths_with_dedicated_code"),
        ],
      }),
      testStep({
        name: "M1-08 合法引用形态到达后端一次（canonicalize + 可访问性）",
        pkg: "aether-tauri",
        testBin: "ipc_validation",
        filter: "valid_requests_reach_backend_exactly_once",
        markers: [okMarker("recovery", "valid_requests_reach_backend_exactly_once")],
      }),
      vitestStep({
        name: "adapter-mock：session.send.references 原样记入 session-log（has_references=真）",
        packageName: "@aether/adapter-mock",
        file: "src/mock-adapter.test.ts",
        testName: "session.send.references",
        markers: [
          {
            phase: "recovery",
            prefix: "session.send.references 原样记入",
            contains: true,
          },
        ],
      }),
    ],
  });
}

// ===== DoD2：P0 30 场景回归 =====

function listEvidenceDirs(task) {
  const base = path.join(repoRoot, "scripts", "test", ".tmp", task);
  if (!existsSync(base)) return [];
  return readdirSync(base)
    .filter((name) => name.startsWith("evidence-"))
    .filter((name) => {
      try {
        return statSync(path.join(base, name)).isDirectory();
      } catch {
        return false;
      }
    })
    .sort();
}

function archiveP0Evidence(evidenceDir, archiveDir) {
  mkdirSync(archiveDir, { recursive: true });
  let copied = 0;
  for (const name of readdirSync(evidenceDir)) {
    if (name === "summary.json" || name.endsWith(".json")) {
      copyFileSync(path.join(evidenceDir, name), path.join(archiveDir, name));
      copied += 1;
    }
  }
  return copied;
}

function runP0Regression(roundDir) {
  const results = [];
  for (const suite of P0_BASELINE) {
    const base = path.join(repoRoot, "scripts", "test", ".tmp", suite.task);
    const before = new Set(listEvidenceDirs(suite.task));
    const started = Date.now();
    console.log(
      `\n########## M8-01 DoD2 P0 回归：${suite.task}（${suite.title}） ##########`,
    );
    const exit = run(node, [
      path.join(repoRoot, "scripts", "test", suite.task, `verify-${suite.task}.mjs`),
    ]);
    const elapsedMs = Date.now() - started;
    const created = listEvidenceDirs(suite.task).filter((name) => !before.has(name));
    const latest = created.at(-1) ?? listEvidenceDirs(suite.task).at(-1) ?? null;
    const evidenceDir = latest ? path.join(base, latest) : null;

    let summary = null;
    let observedIds = [];
    let readError = null;
    if (evidenceDir) {
      try {
        summary = JSON.parse(readFileSync(path.join(evidenceDir, "summary.json"), "utf8"));
        observedIds = (summary.scenarios ?? []).map((scenario) => scenario.scenario);
      } catch (error) {
        readError = error.message;
      }
    }
    const missingIds = suite.ids.filter((id) => !observedIds.includes(id));
    const unexpectedIds = observedIds.filter((id) => !suite.ids.includes(id));
    const countsMatch =
      summary?.total === suite.ids.length &&
      summary?.passed === suite.ids.length &&
      summary?.failed === 0;
    const ok =
      exit === 0 &&
      Boolean(summary) &&
      countsMatch &&
      missingIds.length === 0 &&
      unexpectedIds.length === 0;

    let archived = null;
    if (evidenceDir) {
      const archiveDir = path.join(roundDir, "p0-regression", suite.task);
      try {
        const copied = archiveP0Evidence(evidenceDir, archiveDir);
        archived = { dir: path.relative(repoRoot, archiveDir), files: copied };
      } catch (error) {
        archived = { error: error.message };
      }
    }

    results.push({
      task: suite.task,
      title: suite.title,
      expected: suite.ids.length,
      exit,
      ok,
      elapsed_ms: elapsedMs,
      total: summary?.total ?? null,
      passed: summary?.passed ?? null,
      failed: summary?.failed ?? null,
      missing_ids: missingIds,
      unexpected_ids: unexpectedIds,
      evidence_dir: evidenceDir ? path.relative(repoRoot, evidenceDir) : null,
      archived,
      ...(readError ? { read_error: readError } : {}),
    });
    console.log(
      `[m8-01] P0 ${suite.task}：${ok ? "PASS" : "FAIL"} ` +
        `expected=${suite.ids.length} total=${summary?.total ?? "?"} ` +
        `passed=${summary?.passed ?? "?"} failed=${summary?.failed ?? "?"} exit=${exit}` +
        (missingIds.length > 0 ? ` missing=${missingIds.join(",")}` : "") +
        (unexpectedIds.length > 0 ? ` unexpected=${unexpectedIds.join(",")}` : ""),
    );
  }
  return results;
}

// ===== 轮次执行 =====

function collectFlakes(round, scenarioResults) {
  const flakes = [];
  for (const result of scenarioResults) {
    for (const step of result.steps ?? []) {
      if ((step.attempts ?? 1) > 1) {
        flakes.push({
          round,
          scenario: result.scenario,
          step: step.name,
          attempts: step.attempts,
          exit: step.exit,
          disposition:
            step.exit === 0
              ? "基础设施抖动复跑通过（断言未变；已在证据文档记录）"
              : "失败（该轮不通过；需定位后重跑）",
        });
      }
    }
  }
  return flakes;
}

function listFilesRecursive(root) {
  const files = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else files.push(path.relative(root, full));
    }
  };
  if (existsSync(root)) walk(root);
  return files.sort();
}

async function main() {
  const problems = preflight();
  if (problems.length > 0) {
    console.error("[m8-01] 静态前置检查失败：");
    for (const problem of problems) console.error(`  - ${problem}`);
    process.exit(1);
  }
  console.log("[m8-01] 静态前置检查通过（预注册 / m8-nightly 接线 / package.json / P0 脚本）");

  if (roundsRequired < 3) {
    console.log(
      `[m8-01] 注意：--rounds ${roundsRequired} 为开发定位模式；DoD3 门禁要求 --rounds 3（连续 3 轮全绿）。`,
    );
  }
  if (skipP0) {
    console.log("[m8-01] 注意：--skip-p0 跳过 DoD2 回归；该运行不构成门禁证据。");
  }

  const fixtureOk = buildFixture();
  if (!fixtureOk) {
    console.error("[m8-01] 前置构建失败，终止（禁止静默跳过夹具场景）。");
    process.exit(1);
  }

  const rounds = [];
  const flakes = [];
  for (let index = 1; index <= roundsRequired; index += 1) {
    const roundDir = path.join(runRoot, `round-${index}`);
    const scenariosDir = path.join(roundDir, "scenarios");
    mkdirSync(scenariosDir, { recursive: true });
    console.log(`\n==================== M8-01 夜跑 第 ${index}/${roundsRequired} 轮 ====================`);

    const started = Date.now();
    const drill = createDrill({
      task: "M8-01",
      title: `P1 故障注入与夜跑（第 ${index}/${roundsRequired} 轮）`,
      evidenceDir: scenariosDir,
    });
    defineScenarios(drill, scenariosDir);
    const scenarioResults = await drill.runAll({ only });
    const scenarioSummaryExit = drill.summarize();
    const scenarioOk =
      scenarioResults.length > 0 &&
      scenarioResults.every((result) => result.pass) &&
      (only
        ? true
        : DOD1_SCENARIOS.every((id) =>
            scenarioResults.some((result) => result.scenario === id),
          ));
    flakes.push(...collectFlakes(index, scenarioResults));

    let p0 = [];
    let p0Ok = true;
    if (!skipP0) {
      p0 = runP0Regression(roundDir);
      p0Ok = p0.length === P0_BASELINE.length && p0.every((entry) => entry.ok);
    }

    const roundOk = scenarioOk && p0Ok;
    const roundSummary = {
      round: index,
      ok: roundOk,
      duration_ms: Date.now() - started,
      scenarios: {
        total: scenarioResults.length,
        passed: scenarioResults.filter((result) => result.pass).length,
        failed: scenarioResults.filter((result) => !result.pass).length,
        exit: scenarioSummaryExit,
        evidence_dir: path.relative(repoRoot, scenariosDir),
        artifacts: listFilesRecursive(scenariosDir),
        results: scenarioResults.map((result) => ({
          scenario: result.scenario,
          name: result.name,
          pass: result.pass,
          ...(result.failure_reason ? { failure_reason: result.failure_reason } : {}),
          steps: (result.steps ?? []).map((step) => ({
            name: step.name,
            exit: step.exit,
            attempts: step.attempts,
            elapsed_ms: step.elapsed_ms,
          })),
        })),
      },
      p0_regression: {
        required: !skipP0,
        total: p0.length,
        passed: p0.filter((entry) => entry.ok).length,
        failed: p0.filter((entry) => !entry.ok).length,
        suites: p0,
      },
    };
    writeFileSync(
      path.join(roundDir, "round.json"),
      `${JSON.stringify(roundSummary, null, 2)}\n`,
      "utf8",
    );
    rounds.push(roundSummary);
    console.log(
      `[m8-01] 第 ${index}/${roundsRequired} 轮：${roundOk ? "PASS" : "FAIL"}` +
        `（场景 ${roundSummary.scenarios.passed}/${roundSummary.scenarios.total}；` +
        `P0 ${roundSummary.p0_regression.passed}/${roundSummary.p0_regression.total}）`,
    );
  }

  const roundsPassed = rounds.filter((round) => round.ok).length;
  const dod3 = roundsRequired >= 3 && roundsPassed === roundsRequired;
  const dod1 = rounds.every(
    (round) =>
      round.scenarios.total === DOD1_SCENARIOS.length &&
      round.scenarios.failed === 0 &&
      DOD1_SCENARIOS.every((id) =>
        round.scenarios.results.some((result) => result.scenario === id && result.pass),
      ),
  );
  const dod2 = !skipP0 && rounds.every((round) => round.p0_regression.failed === 0);
  const overall = dod1 && dod2 && dod3;
  const devMode = Boolean(only) || skipP0 || roundsRequired < 3;
  const devOk = rounds.length > 0 && rounds.every((round) => round.ok);

  const summary = {
    task: "M8-01",
    title: "P1 故障注入与夜跑",
    stamp: runStamp,
    platform: process.platform,
    node: process.version,
    rounds_required: roundsRequired,
    rounds_passed: roundsPassed,
    pre_registration: "docs/M8-01-夜跑预注册.md",
    command: `node scripts/test/m8-01/verify-m8-01.mjs --rounds ${roundsRequired}${skipP0 ? " --skip-p0" : ""}${only ? ` --only ${only.join(",")}` : ""}`,
    dod1: {
      ok: dod1,
      scenarios_required: DOD1_SCENARIOS.length,
      scenarios: DOD1_SCENARIOS,
    },
    dod2: {
      ok: dod2,
      skipped: skipP0,
      scenario_total: P0_TOTAL,
      suites: P0_BASELINE.map((suite) => ({ task: suite.task, expected: suite.ids.length })),
    },
    dod3: { ok: dod3, rounds_required: 3, rounds_executed: roundsRequired, rounds_passed: roundsPassed },
    ok: overall,
    dev_mode: devMode,
    dev_ok: devOk,
    flakes,
    rounds,
  };
  writeFileSync(path.join(runRoot, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`, "utf8");
  writeFileSync(
    path.join(runRoot, "flake-ledger.json"),
    `${JSON.stringify({ task: "M8-01", stamp: runStamp, flakes }, null, 2)}\n`,
    "utf8",
  );

  console.log("\n===== M8-01 P1 故障注入与夜跑 =====");
  console.log(`DoD1 九场景三段证据：${dod1 ? "PASS" : "FAIL"}（${DOD1_SCENARIOS.length} 场景）`);
  console.log(
    `DoD2 P0 30 场景回归：${skipP0 ? "SKIP（--skip-p0，非门禁证据）" : dod2 ? "PASS" : "FAIL"}（${P0_TOTAL} 场景/轮）`,
  );
  console.log(
    `DoD3 连续 ≥3 轮夜跑：${dod3 ? "PASS" : "FAIL"}（${roundsPassed}/${roundsRequired} 轮通过；门禁要求 3/3）`,
  );
  console.log(`flake 记录：${flakes.length} 条（见 flake-ledger.json）`);
  console.log(`[m8-01] 证据目录：${runRoot}`);
  console.log(
    `----- ${overall ? "全部通过" : devMode && devOk ? "开发定位通过（非门禁证据）" : "存在未通过项"} -----`,
  );
  process.exit(overall || (devMode && devOk) ? 0 : 1);
}

await main();
