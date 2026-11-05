#!/usr/bin/env node
/**
 * M9-14 DoD-2：G4/G5/G6/G7 失败处理演练（ADR-029 v0.4 D-029-12）。
 *
 * 口径：
 *   - 任一 Gate 失败 → **停止发布**；
 *   - 修复后**重跑该 Gate**，通过才可继续；
 *   - 可保留上一 Electron 候选构建；**不回装/不生成 Tauri 回退包**；
 *   - 演练结果归档 `scripts/test/.tmp/m9-14/failure-drill.json`。
 *
 * 本脚本不修改真实 Gate 证据/阈值/断言，只演练发布决策模型：
 * 对 G4–G7 的通过/失败状态做注入 → 断言「停止发布」→ 恢复 → 断言「重跑后放行」。
 *
 * 用法：
 *   node scripts/test/m9-14/failure-drill.mjs [--out <file>]
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const outPath = path.resolve(
  repoRoot,
  process.argv.includes("--out")
    ? process.argv[process.argv.indexOf("--out") + 1]
    : path.join("scripts", "test", ".tmp", "m9-14", "failure-drill.json"),
);
const packageJson = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));

/** G4–G7 的发布前通过条件（与 P2 计划 §2.15 / ADR-029 §10 对应）。 */
const GATES = [
  {
    id: "G4",
    name: "Electron 壳安全基线",
    command: "pnpm verify:m9-07 && pnpm verify:m9-08 && pnpm verify:security",
    failure: "安全基线失败（preload/CSP/导航/隔离）",
    fault: { key: "security_baseline", value: false },
  },
  {
    id: "G5",
    name: "数据/安全（旧库、备份、Keychain、路径）",
    command: "pnpm verify:m9-09 && pnpm verify:security && pnpm m9-13:data",
    failure: "数据丢失或 Keychain 不可恢复",
    fault: { key: "data_and_keychain", value: false },
  },
  {
    id: "G6",
    name: "打包/签名/安装冒烟",
    command: "pnpm package:desktop && pnpm smoke:installer && pnpm verify:m9-11",
    failure: "安装/签名/公证失败",
    fault: { key: "installer_and_signing", value: false },
  },
  {
    id: "G7",
    name: "全量回归/性能/基线",
    command: "pnpm verify:m9-13",
    failure: "P0/P1 回归或硬阈值失败",
    fault: { key: "regression_and_thresholds", value: false },
  },
];

/** 发布候选：Electron 为唯一候选；无 Tauri 回退包。 */
function candidateState(overrides = {}) {
  return {
    electron: {
      version: packageJson.version,
      // 允许保留上一 Electron 候选；不是 Tauri 回退包。
      previous_candidate_retainable: true,
    },
    tauri_package: null,
    gates: {
      G4: { passed: true, security_baseline: true },
      G5: { passed: true, data_and_keychain: true },
      G6: { passed: true, installer_and_signing: true },
      G7: { passed: true, regression_and_thresholds: true },
    },
    ...overrides,
  };
}

/** 发布决策：任何 Gate 失败即停止发布；永不选择 Tauri。 */
function decideRelease(state) {
  const failed = [];
  for (const gate of GATES) {
    const evidence = state.gates?.[gate.id] ?? { passed: false };
    const faultKey = gate.fault.key;
    const faulted = evidence[faultKey] === false;
    if (evidence.passed !== true || faulted) failed.push(gate.id);
  }
  if (failed.length > 0) {
    return {
      decision: "stop_release",
      failed_gates: failed,
      rerun_gates: failed,
      fallback: state.electron?.previous_candidate_retainable
        ? "previous_electron_candidate"
        : "none",
      tauri_fallback: false,
    };
  }
  return {
    decision: "release",
    failed_gates: [],
    rerun_gates: [],
    fallback: "none",
    tauri_fallback: false,
  };
}

function rejectTauriFallback() {
  return {
    requested: "tauri",
    allowed: false,
    error: "tauri_fallback_forbidden",
  };
}

const steps = [];
let failed = 0;

function record(name, ok, detail = "") {
  steps.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
  if (!ok) failed += 1;
}

// 0) 基线：四 Gate 全绿 → 可发布。
const baseline = decideRelease(candidateState());
record(
  "基线：G4/G5/G6/G7 全绿 → 允许发布",
  baseline.decision === "release" && baseline.tauri_fallback === false,
  JSON.stringify(baseline),
);

// 1) 逐 Gate 注入失败 → 停止发布 → 修复 → 重跑通过。
for (const gate of GATES) {
  const faulty = candidateState();
  faulty.gates[gate.id] = { passed: false, [gate.fault.key]: false };
  const stopped = decideRelease(faulty);
  record(
    `${gate.id} 失败（${gate.failure}）→ 停止发布`,
    stopped.decision === "stop_release" &&
      stopped.failed_gates.includes(gate.id) &&
      stopped.rerun_gates.includes(gate.id) &&
      stopped.fallback !== "tauri",
    JSON.stringify(stopped),
  );
  record(
    `${gate.id} 失败 → 不生成/不回装 Tauri 回退包`,
    stopped.tauri_fallback === false && rejectTauriFallback().allowed === false,
    rejectTauriFallback().error,
  );
  record(
    `${gate.id} 修复后重跑 Gate → 通过并放行`,
    (() => {
      const fixed = candidateState();
      const rerun = decideRelease(fixed);
      return (
        rerun.decision === "release" &&
        rerun.failed_gates.length === 0 &&
        rerun.tauri_fallback === false
      );
    })(),
    `${gate.command}`,
  );
}

// 2) 多 Gate 同时失败 → 全部列入重跑清单，仍无 Tauri。
const multi = candidateState();
multi.gates.G5 = { passed: false, data_and_keychain: false };
multi.gates.G7 = { passed: false, regression_and_thresholds: false };
const multiStopped = decideRelease(multi);
record(
  "多 Gate 失败 → 停止发布并列出全部重跑项",
  multiStopped.decision === "stop_release" &&
    multiStopped.failed_gates.includes("G5") &&
    multiStopped.failed_gates.includes("G7") &&
    multiStopped.tauri_fallback === false,
  JSON.stringify(multiStopped),
);

// 3) 上一 Electron 候选可保留；Tauri 候选不存在。
record(
  "可保留上一 Electron 候选；无 Tauri 候选",
  candidateState().electron.previous_candidate_retainable === true &&
    candidateState().tauri_package === null,
  "previous_electron_candidate",
);

const report = {
  schema: "aether.m9-14.failure-drill.v1",
  task: "M9-14",
  generated_at: new Date().toISOString(),
  policy: {
    gate_failure: "stop_release",
    fix: "fix_then_rerun_gate",
    fallback: "previous_electron_candidate_only",
    tauri_fallback: "forbidden",
  },
  gates: GATES.map((gate) => ({
    id: gate.id,
    name: gate.name,
    command: gate.command,
    failure: gate.failure,
  })),
  steps,
  passed: failed === 0,
};
mkdirSync(path.dirname(outPath), { recursive: true });
writeFileSync(outPath, `${JSON.stringify(report, null, 2)}\n`, "utf8");
console.log(`[m9-14-failure-drill] 证据：${path.relative(repoRoot, outPath)}`);
if (failed > 0) {
  console.error(`[m9-14-failure-drill] FAIL ${failed} 项`);
  process.exit(1);
}
console.log("AETHER_M9_14_FAILURE_DRILL PASS");
