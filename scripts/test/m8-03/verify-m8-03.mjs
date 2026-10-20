#!/usr/bin/env node
/**
 * M8-03 验证入口：P1 门禁验收执行（阶段退出标准）。
 *
 * 覆盖：
 * - 退出标准 1–3 证据（M7-05 / M5-03 / M6-02）与 17 项门禁任务证据索引；
 * - 直接实施项 M7-07/08/09 单独索引、不参与门禁判定；
 * - P0 T1–T15 关键回归（T1–T4/T6/T7/T11/T14；其余见证据索引）；
 * - 数据事故零记录（M8-01 最新单轮 summary：9/9 + 30/30 + flake 0）；
 * - 当前 HEAD 加密备份恢复演练（退出标准 2 复跑）。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m8-03", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
const assert = (condition, message) => {
  if (!condition) throw new Error(message);
};

// ===== 1. 静态检查：证据/索引/退出标准/数据事故 =====
try {
  assert(existsSync(path.join(repoRoot, "docs/M8-03-证据.md")), "缺少 docs/M8-03-证据.md");
  assert(
    existsSync(path.join(repoRoot, "docs/M8-03-Gate-验收报告.md")),
    "缺少 docs/M8-03-Gate-验收报告.md",
  );
  const evidence = read("docs/M8-03-证据.md");
  const report = read("docs/M8-03-Gate-验收报告.md");

  for (const task of [
    "M5-01", "M5-02", "M5-03", "M5-04", "M5-05",
    "M6-01", "M6-02", "M6-03",
    "M7-01", "M7-02", "M7-03", "M7-04", "M7-05", "M7-06",
    "M8-01", "M8-03", "M8-04",
  ]) {
    assert(evidence.includes(`| ${task} |`), `门禁索引缺 ${task}`);
  }
  for (const task of ["M7-07", "M7-08", "M7-09"]) {
    assert(evidence.includes(task), `直接实施项索引缺 ${task}`);
  }
  assert(evidence.includes("直接实施项（不参与门禁判定）"), "缺少直接实施项独立索引声明");
  assert(evidence.includes("P1 门禁通过"), "证据缺少门禁结论");
  assert(report.includes("PASS（P1 门禁通过）"), "报告缺少 PASS 结论");

  for (const [label, file, token] of [
    ["M7-05", "docs/M7-05-证据.md", "22/22"],
    ["M5-03", "docs/M5-03-证据.md", "两轮均全绿"],
    ["M6-02", "docs/M6-02-证据.md", "1.00%"],
  ]) {
    assert(existsSync(path.join(repoRoot, file)), `退出标准证据缺失：${file}`);
    assert(read(file).includes(token), `${label} 证据缺少关键结论「${token}」`);
  }

  // M8-01 最新单轮：9/9 + 30/30 + flake 0（数据事故零记录的输入）。
  const runsDir = path.join(repoRoot, "scripts", "test", ".tmp", "m8-01");
  assert(existsSync(runsDir), "缺少 M8-01 夜跑证据目录");
  const latestRun = readdirSync(runsDir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && entry.name.startsWith("run-"))
    .map((entry) => entry.name)
    .sort()
    .at(-1);
  assert(latestRun, "M8-01 无 run-* 证据目录");
  const summary = JSON.parse(
    readFileSync(path.join(runsDir, latestRun, "summary.json"), "utf8"),
  );
  assert(summary.dod1?.ok === true, `M8-01 最新轮 P1 场景未全绿（${latestRun}）`);
  assert(summary.dod2?.ok === true, `M8-01 最新轮 P0 30 场景未全绿（${latestRun}）`);
  assert(Array.isArray(summary.flakes) && summary.flakes.length === 0, "M8-01 flake 台账非空");

  // T1–T15 表：T5a/T5b 与 T8/T9 同行，至少 14 行覆盖 15 个用例。
  const tRows = (evidence.match(/\|\s*T\d+[a-b]?[\s/|]/g) ?? []).length;
  assert(tRows >= 14, `P0 T1–T15 证据行不足（${tRows}）`);

  record(
    `静态：退出标准 1–3 / 17 项门禁索引 / 直接实施项独立 / T1–T15 / M8-01 最新轮 ${latestRun}`,
    0,
  );
} catch (failure) {
  record(`静态检查失败：${failure && failure.message ? failure.message : failure}`, 1);
}

// ===== 2. 退出标准 2 复跑：加密备份恢复演练 =====
record(
  "退出标准 2 复跑 m5-03 加密备份恢复演练（默认 2 轮）",
  run(process.execPath, [path.join(repoRoot, "scripts/test/m5-03/restore-drill.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 3. P0 关键回归：T1–T4 / T6 / T7 / T11 =====
record(
  "P0 T1–T3 m4_04_t1_t3（创建时延 / 回显时延 / 10 并发零丢失）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m4_04_t1_t3"], { cwd: repoRoot }),
);
record(
  "P0 T4 m3_06_t4（kill -9 ×20 已确认消息零丢失）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_06_t4"], { cwd: repoRoot }),
);
record(
  "P0 T11 m2_08_exit（退出 10s 预算 / 无孤儿）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m2_08_exit"], { cwd: repoRoot }),
);
record(
  "P0 T6/T7 m2_03_permission + m6_02_rule_safety（100 并发 / 路径逃逸 / 零直通）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-control",
      "--test",
      "m2_03_permission",
      "--test",
      "m6_02_rule_safety",
    ],
    { cwd: repoRoot },
  ),
);

// ===== 4. T14 类型契约 + 迁移/附录 C =====
record(
  "T14 bindings 重新生成 + git diff --exit-code",
  run(process.execPath, [path.join(repoRoot, "scripts/ci/bindings.mjs"), "check"]),
);
record(
  "迁移 0001–0008 ↔ 附录 C（verify:m1-03）",
  run(process.execPath, [path.join(repoRoot, "scripts/test/m1-03/verify-m1-03.mjs")]),
);

// ===== 5. 前端全量（T1/T2/T3/T4/T12 的 UI 面回归）=====
record(
  "前端全量（@aether/desktop test）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "test"]),
);

const summary = {
  task: "M8-03",
  generated_at: new Date().toISOString(),
  evidence_dir: path.relative(repoRoot, evidenceDir),
  checks,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m8-03] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m8-03", checks));
