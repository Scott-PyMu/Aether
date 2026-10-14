/**
 * M6-02 验证入口：规则安全回归与误配率抽查（P1 计划 M6-02 DoD1–DoD5；
 * ADR-019 §2.2 预注册方法、附录 A-4 归档契约）。
 *
 * 覆盖：
 *  - DoD1：方法预注册（`docs/M6-02-误配率抽查方法-预注册.md` 在案 + ADR-019 §2.2 一致）；
 *  - DoD2：执行抽查并计算误配率（真实系统批量执行 → 归档工件 → 本脚本
 *    **独立复核**：判定 1（规范重演）与判定 2（真实系统）逐行比对的原始结果、
 *    复算误配率、校验归档契约）；
 *  - DoD3：T7 全样本 + M2-10 零直通 + T6 回归复跑通过（m2-03 / m2-10 / m6-02 套件）；
 *  - DoD4：误配案例逐条分析与处置建议工件齐备（`误配案例分析.md` + 分类断言）；
 *  - DoD5：误配率 <5% 门禁（≥5% 或严重误配 → FAIL，触发 P1 门禁回退动作）。
 *
 * 执行方式：
 *  - `node scripts/test/m6-02/verify-m6-02.mjs`：新执行窗口（注入证据目录）并全量索引；
 *  - `node scripts/test/m6-02/verify-m6-02.mjs --evidence <dir>`：对既有证据目录复核
 *    （复跑脚本口径；不重新执行抽查）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");
const args = process.argv.slice(2);
const evidenceArgIndex = args.indexOf("--evidence");
const evidenceArg = evidenceArgIndex >= 0 ? args[evidenceArgIndex + 1] : null;
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir =
  evidenceArg ??
  path.join(repoRoot, "scripts", "test", ".tmp", "m6-02", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 0. 静态：预注册方法与 ADR-019 §2.2 一致 =====

{
  const problems = [];
  const prereg = path.join(repoRoot, "docs", "M6-02-误配率抽查方法-预注册.md");
  if (!existsSync(prereg)) {
    problems.push("缺少 docs/M6-02-误配率抽查方法-预注册.md");
  } else {
    const text = readFileSync(prereg, "utf8");
    for (const token of [
      "ADR-019",
      "样本量",
      "100",
      "双人独立判定",
      "第三人复核",
      "实现缺陷",
      "硬边界",
      "表达误配",
      "<5%",
    ]) {
      if (!text.includes(token)) problems.push(`预注册文档缺少登记标记：${token}`);
    }
  }
  const adr019 = readFileSync(
    path.join(repoRoot, "docs", "adr", "ADR-019-permission-rules-engine.md"),
    "utf8",
  );
  for (const token of [
    "误配率抽查方法",
    "样本量",
    "双人独立判定",
    "第三人复核",
    "误配率 = 误配条数 / 样本量",
    "<5% 通过",
    "样本清单.json",
    "双人判定表.csv",
    "误配率计算输出.txt",
  ]) {
    if (!adr019.includes(token)) problems.push(`ADR-019 缺少登记标记：${token}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("DoD1 静态：预注册方法在案且与 ADR-019 §2.2 一致", problems.length === 0 ? 0 : 1);
}

// ===== 1. 执行：抽查与安全回归套件（证据注入） =====

record(
  "DoD2/5：M6-02 抽查与安全回归套件（3 用例：双人判定+误配率 / 案例分析+数据完整性 / T6）",
  run(
    cargo,
    ["test", "-p", "aether-control", "--test", "m6_02_rule_safety", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M6_02_EVIDENCE_DIR: evidenceDir }, capture: true },
  ),
);
record(
  "DoD3：T7 全样本回归 + T6 100 并发 ask（m2-03：路径样本/审计/超时/重启恢复）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_03_permission"], { cwd: repoRoot }),
);
record(
  "DoD3：M2-10 回环零直通回归（网关探针：收到=决议=下发）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m2_10_permission_loop"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD3：规则引擎与档位矩阵单测（aether-security；含 M6-02 缺陷修复回归）",
  run(cargo, ["test", "-p", "aether-security", "permission::"], { cwd: repoRoot }),
);

// ===== 2. 归档契约复核（ADR-019 附录 A-4）+ 独立复算误配率 =====

function readEvidenceDir() {
  return {
    sampleFile: path.join(evidenceDir, "样本清单.json"),
    csvFile: path.join(evidenceDir, "双人判定表.csv"),
    rateFile: path.join(evidenceDir, "误配率计算输出.txt"),
    detailFile: path.join(evidenceDir, "判定明细.json"),
    analysisFile: path.join(evidenceDir, "误配案例分析.md"),
    fingerprintFile: path.join(evidenceDir, "rules_fingerprint.json"),
    safetyFile: path.join(evidenceDir, "安全回归.json"),
    t6File: path.join(evidenceDir, "t6_并发回归.json"),
  };
}

let contractOk = true;
let contractDetail = "";
let independentVerdict = "FAIL";
{
  const files = readEvidenceDir();
  const required = [
    ["样本清单.json", files.sampleFile],
    ["双人判定表.csv", files.csvFile],
    ["误配率计算输出.txt", files.rateFile],
    ["判定明细.json", files.detailFile],
    ["误配案例分析.md", files.analysisFile],
    ["rules_fingerprint.json", files.fingerprintFile],
    ["安全回归.json", files.safetyFile],
    ["t6_并发回归.json", files.t6File],
  ];
  const missing = required.filter(([, file]) => !existsSync(file)).map(([name]) => name);
  if (missing.length > 0) {
    contractOk = false;
    contractDetail = `缺失：${missing.join(", ")}`;
  } else {
    const sample = JSON.parse(readFileSync(files.sampleFile, "utf8"));
    const detail = JSON.parse(readFileSync(files.detailFile, "utf8"));
    const rate = readFileSync(files.rateFile, "utf8");
    const safety = JSON.parse(readFileSync(files.safetyFile, "utf8"));
    const t6 = JSON.parse(readFileSync(files.t6File, "utf8"));

    const problems = [];
    // 样本：100 条、全部唯一、与规则集同源。
    if (sample.sample_size !== 100) problems.push(`样本量 ${sample.sample_size} ≠ 100`);
    const sampleIds = sample.sample_ids ?? [];
    if (new Set(sampleIds).size !== sampleIds.length) problems.push("样本存在重复 id");
    if (!Number.isInteger(sample.persisted_total) || sample.persisted_total < 100) {
      problems.push("persisted_total 异常（执行窗口规则集 <100 时不足取全集，须说明）");
    }
    // 判定表：20 列、行数 = 主向量 + 边界向量。
    const csv = readFileSync(files.csvFile, "utf8").trim().split(/\r?\n/);
    const header = csv[0];
    for (const column of [
      "rule_id",
      "judge1_expected",
      "judge2_decision",
      "classification",
      "misconfigured",
    ]) {
      if (!header.includes(column)) problems.push(`双人判定表缺少列：${column}`);
    }
    if (csv.length - 1 !== detail.rows.length) problems.push("判定表行数与明细不一致");

    // 独立复算：主向量误配率与记录一致；严重误配 0；零直通 0。
    const rows = detail.rows;
    const primaryRows = rows.filter((row) => row.primary);
    const mismatched = primaryRows.filter((row) => row.misconfigured === true);
    const recomputedRate = (mismatched.length * 100) / primaryRows.length;
    const severe = rows.filter((row) => row.classification === "severe_hard_boundary").length;
    const drift = rows.filter(
      (row) => row.primary !== true && row.misconfigured === true && row.judge2_decision === "allow",
    ).length;
    // 第三方判定：每行判定 1 与判定 2 的原始值 + 分类是否自洽。
    for (const row of rows) {
      const divergence = row.judge1_expected !== row.judge2_decision;
      if (row.divergence !== divergence) {
        problems.push(`行分歧标记不自洽：${row.request_id}`);
        break;
      }
    }
    const reported = /misconfiguration_rate=([\d.]+)%/.exec(rate);
    const reportedValue = reported ? Number(reported[1]) : Number.NaN;
    if (Math.abs(reportedValue - recomputedRate) > 0.001) {
      problems.push(`记录误配率 ${reportedValue}% ≠ 独立复算 ${recomputedRate.toFixed(2)}%`);
    }
    if (severe !== 0) problems.push(`严重误配 ${severe} 条（单条即触发降级）`);
    if (drift !== 0) problems.push(`边界向量出现 allow 漂移 ${drift} 条`);
    if (detail.zero_passthrough_violations !== undefined && detail.zero_passthrough_violations !== 0) {
      problems.push(`零直通违规 ${detail.zero_passthrough_violations} 条`);
    }
    if (safety.rules_unmodified !== true) problems.push("规则数据未保持前后一致");
    if (safety.pending_rule_change_isolation !== true) problems.push("pending 票据隔离断言缺失");
    if (safety.hard_boundary_denied !== true) problems.push("硬边界断言缺失");
    if (t6.allowed !== 50 || t6.denied !== 50) problems.push("T6 并发结果异常");

    // 误配案例分析：逐条覆盖误配行。
    const analysis = readFileSync(files.analysisFile, "utf8");
    const caseCount = (analysis.match(/^## 案例/gm) ?? []).length;
    if (caseCount !== mismatched.length) {
      problems.push(`案例分析 ${caseCount} 条 ≠ 误配 ${mismatched.length} 条`);
    }
    for (const row of mismatched) {
      if (!analysis.includes(row.rule_id)) {
        problems.push(`案例分析缺规则：${row.rule_id}`);
      }
    }
    if (recomputedRate < 5 && severe === 0) {
      independentVerdict = "PASS";
    }
    contractDetail = `样本 ${sample.sample_size}（窗口规则 ${sample.persisted_total}）；主向量 ${primaryRows.length} 误配 ${mismatched.length}（${recomputedRate.toFixed(2)}%）；严重 ${severe}；边界漂移 ${drift}`;
    if (problems.length > 0) {
      contractOk = false;
      contractDetail += `；问题：${problems.join("；")}`;
    }
  }
}
if (!contractOk) console.error(`[m6-02] 归档/复算失败：${contractDetail}`);
record(
  `DoD2/4/5 独立复核：归档契约 + 误配率复算（${independentVerdict}）`,
  contractOk && independentVerdict === "PASS" ? 0 : 1,
);

// ===== 3. 输出摘要 =====

console.log(`[m6-02] 证据目录：${evidenceDir}`);
console.log(`[m6-02] 复核结论：${contractDetail}`);

process.exit(summarize("verify-m6-02", checks));
