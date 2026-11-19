#!/usr/bin/env node
/**
 * `pnpm verify:m10-13` —— P2 收口 / G13 证据汇总门禁（M10-13 DoD-1～DoD-10）。
 *
 * 默认检查静态证据 + 已归档的 P0/P1 复跑结果；`--run-gates` 额外串行执行
 * `verify:m10-09/10/11/14`、`verify:m10-14:level1/:level2`。
 *
 * 用法：
 *   node scripts/test/m10-13/verify-m10-13.mjs [--run-gates]
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { pnpmCommand } from "../lib/exec.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const runGates = process.argv.includes("--run-gates");
const failures = [];
const checks = [];

function check(name, ok, detail = "") {
  checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `：${detail}` : ""}`);
  if (!ok) failures.push(name);
}

const read = (rel) => readFileSync(path.join(root, rel), "utf8");
const json = (rel) => JSON.parse(read(rel));
const exists = (rel) => existsSync(path.join(root, rel));
const pnpm = pnpmCommand();

/** 证据路径支持 `*` 通配（如 dist-out/m9-11-smoke-test-*.json）。 */
function evidenceExists(rel) {
  if (!rel.includes("*")) return exists(rel);
  const directory = path.dirname(rel);
  const pattern = path.basename(rel);
  const regex = new RegExp(
    `^${pattern
      .split("*")
      .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
      .join(".*")}$`,
  );
  return (
    existsSync(path.join(root, directory)) &&
    readdirSync(path.join(root, directory)).some((name) => regex.test(name))
  );
}

function run(label, command, args, options = {}) {
  console.log(`\n$ ${[command, ...args].join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: "inherit",
    shell: process.platform === "win32" && /\.(cmd|bat)$/i.test(command),
    env: {
      ...process.env,
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
      CARGO_PROFILE_TEST_DEBUG: process.env.CARGO_PROFILE_TEST_DEBUG ?? "0",
      CARGO_PROFILE_TEST_CODEGEN_UNITS: process.env.CARGO_PROFILE_TEST_CODEGEN_UNITS ?? "1",
      CARGO_INCREMENTAL: process.env.CARGO_INCREMENTAL ?? "0",
      ...(options.env ?? {}),
    },
    timeout: options.timeout ?? 60 * 60 * 1000,
  });
  check(label, result.status === 0, result.status === null ? String(result.error?.message ?? "") : `exit=${result.status}`);
}

// ── 1. 任务表 / 依赖图 / 证据链接（DoD-1） ──────────────────────────────────
const plan = read("docs/P2-架构迁移-实施计划与验收标准.md");
check("P2 计划 v0.15", plan.includes("v0.15") && plan.includes("计划版本"));
const headings = [...plan.matchAll(/^### (M10-\d{2})\b/gm)].map((match) => match[1]);
const expectedTasks = [
  "M10-00",
  "M10-01",
  "M10-02",
  "M10-03",
  "M10-04",
  "M10-05",
  "M10-06",
  "M10-07",
  "M10-08",
  "M10-09",
  "M10-10",
  "M10-11",
  "M10-13",
  "M10-14",
];
check("M10 任务标题 = 14 项", headings.length === 14, headings.join(","));
check(
  "M10 任务集合完整（M10-12 保留）",
  expectedTasks.every((task) => headings.includes(task)) &&
    !headings.includes("M10-12") &&
    plan.includes("M10-12 保留"),
);
check(
  "依赖链 M10-08 → M10-09 → M10-10/M10-11 → M10-14 → M10-13",
  plan.includes("M10-08 → M10-09 → M10-10/M10-11 → M10-14 → M10-13"),
);
check("任务总量 14 项", plan.includes("M10：**14 项**"));
check("M10-13 DoD 引用 verify:m10-13", plan.includes("verify:m10-13"));
check(
  "P2 计划记录 M10-13 已执行 / G13 签核",
  /M10-13[^\n]{0,80}(已执行|收口)/.test(plan) &&
    /G13[^\n]{0,80}(conditional_pass|待)/.test(plan),
);

const evidenceDocs = [
  "docs/M10-00-证据.md",
  "docs/M10-01-证据.md",
  "docs/M10-02-证据.md",
  "docs/M10-03-证据.md",
  "docs/M10-04-证据.md",
  "docs/M10-05-证据.md",
  "docs/M10-06-证据.md",
  "docs/M10-07-证据.md",
  "docs/M10-08-证据.md",
  "docs/M10-09-证据.md",
  "docs/M10-10-证据.md",
  "docs/M10-11-证据.md",
  "docs/M10-13-证据.md",
  "docs/M10-10-补充证据.md",
];
for (const doc of evidenceDocs) check(`证据存在 ${doc}`, exists(doc));
check("M10-10 补充分册 metrics", exists("docs/M10-10-补充-metrics.json"));
check("M10-08 报告归档 metrics", exists("docs/M10-08-metrics.json"));
check("M10-08 并行 quick_check 产物", exists("docs/M10-08-quick-check-parallel.json"));
check("M10-13 G13 报告存在", exists("docs/M10-Gate-G13-报告.md"));

// ── 2. 版本 / ADR / 基线（DoD-4） ──────────────────────────────────────────
const designHead = read("设计文档.md").split(/\r?\n/).slice(0, 40).join("\n");
const planHead = plan.split(/\r?\n/).slice(0, 12).join("\n");
const uiuxHead = read("docs/UI-UX-设计规格.md").split(/\r?\n/).slice(0, 12).join("\n");
check("设计文档 v1.36", designHead.includes("v1.36"));
check("P2 计划 v0.15", planHead.includes("v0.15"));
check("UI-UX v0.21", uiuxHead.includes("v0.21"));
const adr039 = read("docs/adr/ADR-039-m10-event-sharding-and-startup-optimization.md");
const adr040 = read("docs/adr/ADR-040-selective-shard-startup-check.md");
check("ADR-039 v1.1", adr039.includes("v1.1"));
check("ADR-039 amended by ADR-040", adr039.includes("amended by ADR-040"));
check("ADR-040 v1.0", adr040.includes("v1.0"));

const baseline = json("docs/M9-01-baseline/manifest.json");
check("M9-01 基线 commands=87", baseline.counts?.commands === 87, String(baseline.counts?.commands));
check("M9-01 基线 daemon=83 / shell=4", baseline.counts?.daemon === 83 && baseline.counts?.shell === 4);
check("M9-01 基线业务码=35", baseline.counts?.business_codes === 35);
check("M9-01 基线设置键=11", baseline.counts?.settings_keys === 11);
check("M9-01 基线迁移=14", baseline.counts?.migrations === 14);
const manifest = json("packages/protocol/src/command-manifest.json");
check("命令面 87", (manifest.commands ?? manifest).length === 87);
const errorCodes = json("packages/protocol/src/error_codes.json");
check("IpcErrorCode 32", errorCodes.ipc?.length === 32);
check("业务码 35", errorCodes.business?.length === 35);
const migrations = readdirSync(path.join(root, "migrations")).filter((name) => /^\d{4}_.*\.sql$/.test(name));
check("迁移计数 14", migrations.length === 14, String(migrations.length));
check("0014 存在且 0015 不存在", migrations.includes("0014_session_shard.sql") && !migrations.includes("0015_session_shard.sql"));

// ── 3. M10-03 发布门（DoD-5） ──────────────────────────────────────────────
const releaseGate = read("docs/M10-13-发布门.md");
for (const marker of ["消费者清单", "升级计划", "旧 v1", "新 v2", "发布说明", "contract_version", "full_coverage"]) {
  check(`发布门含「${marker}」`, releaseGate.includes(marker));
}
check("发布门引用 health-contract-regression", releaseGate.includes("health-contract-regression.mjs"));
check("发布门引用 verify:m10-03", releaseGate.includes("verify:m10-03"));
check("发布门引用 verify:api-schema", releaseGate.includes("verify:api-schema"));

// ── 4. P0/P1 复跑证据（DoD-6） ─────────────────────────────────────────────
const p0p1 = json("docs/M10-13-p0-p1.json");
const entries = p0p1.entries ?? [];
const ids = new Set(entries.map((entry) => entry.id));
for (let index = 1; index <= 15; index += 1) {
  const id = `T${index}`;
  const covered = index === 5 ? ids.has("T5a") && ids.has("T5b") : ids.has(id);
  check(`P0 复跑覆盖 ${id}`, covered);
}
for (const id of ["P1-三运行时一致性", "P1-加密备份恢复", "P1-规则误配率", "M1-08", "M4-05"]) {
  check(`复跑覆盖 ${id}`, ids.has(id));
}
for (const entry of entries) {
  check(
    `${entry.id} 状态`,
    entry.status === "passed" || (entry.status === "ci-pass" && typeof entry.ci_run === "string"),
    `${entry.status ?? "缺失"}`,
  );
  const evidencePaths = String(entry.evidence ?? "")
    .split(";")
    .map((item) => item.trim().split("#")[0].split("\\").join("/"))
    .filter(Boolean);
  check(`${entry.id} 有证据`, evidencePaths.length > 0);
  for (const rel of evidencePaths) {
    check(`${entry.id} 证据存在 ${rel}`, evidenceExists(rel));
  }
}
const t15 = entries.find((entry) => entry.id === "T15");
check("T15 使用 CI Coverage gate", t15?.status === "ci-pass" && t15?.ci_run === "44332199127");

// ── 5. 文档冻结清单漂移处理（DoD-8） ───────────────────────────────────────
const freeze = json("docs/M10-文档冻结清单.json");
check("冻结清单设计 v1.36", freeze.design_version === "v1.36", freeze.design_version);
check("冻结清单 P2 v0.15", freeze.p2_plan_version === "v0.15", freeze.p2_plan_version);
check("冻结清单 UI-UX v0.21", freeze.uiux_version === "v0.21", freeze.uiux_version);
check("冻结清单含 ADR-039/040", (freeze.artifacts ?? []).some((entry) => entry.path.includes("ADR-039")) && (freeze.artifacts ?? []).some((entry) => entry.path.includes("ADR-040")));
check("冻结清单修正记录 M10-13", (freeze.amendments ?? []).some((entry) => entry.task === "M10-13"));

// ── 6. 归档清单（DoD-2/DoD-4） ─────────────────────────────────────────────
const archivePath = "docs/M10-13-归档清单.json";
check("归档清单存在", exists(archivePath));
if (exists(archivePath)) {
  const archive = json(archivePath);
  check("归档 schema", archive.schema === "aether.m10-13.archive.v1");
  check("归档版本 v1.36/v0.15/v0.21", archive.versions?.design === "v1.36" && archive.versions?.p2 === "v0.15" && archive.versions?.uiux === "v0.21");
  for (const artifact of archive.artifacts ?? []) {
    if (!exists(artifact.path)) {
      check(`归档文件存在 ${artifact.path}`, false);
      continue;
    }
    const actual = createHash("sha256").update(readFileSync(path.join(root, artifact.path))).digest("hex");
    check(`归档哈希 ${artifact.path}`, actual === artifact.sha256, `${actual.slice(0, 12)} vs ${String(artifact.sha256).slice(0, 12)}`);
  }
}

// ── 7. G13 报告口径（DoD-7/DoD-9） ─────────────────────────────────────────
const deviceProbePath = "docs/M10-14-device-probe.json";
check("M10-14 设备探针存在", exists(deviceProbePath));
if (exists(deviceProbePath)) {
  const probe = json(deviceProbePath);
  check("M10-14 设备探针 passed", probe.passed === true);
  check(
    "M10-14 记录 NVMe BusType",
    (probe.physical_disks ?? []).some(
      (disk) => String(disk.BusType ?? "").toLowerCase() === "nvme",
    ),
  );
  check(
    "M10-14 覆盖 nvme/8",
    probe.override?.device === "nvme" &&
      probe.override?.concurrency === 8 &&
      probe.override?.source === "env:device",
  );
}
check("M10-14 冷缓存证据存在", exists("docs/M10-14-cold-cache.json"));

const g13 = read("docs/M10-Gate-G13-报告.md");
check("G13 区分 startupCheckCompletedAt", g13.includes("startupCheckCompletedAt"));
check("G13 区分 fullCoverageCheckCompletedAt", g13.includes("fullCoverageCheckCompletedAt"));
check("G13 含 SATA SSD 边界", g13.includes("SATA SSD"));
check("G13 含 HDD 边界", g13.includes("HDD"));
check("G13 含 verify:m10-00 漂移处理", g13.includes("verify:m10-00"));
check(
  "G13 签核状态（conditional_pass 或待签核）",
  (g13.includes("conditional_pass") && g13.includes("2026-11-19")) ||
    (g13.includes("独立签核") && (g13.includes("待") || g13.includes("未签核"))),
);

// ── 8. 可执行门禁 ──────────────────────────────────────────────────────────
run("verify:m10-00（冻结清单/生成物/迁移）", pnpm.command, [...pnpm.prefix, "verify:m10-00"], { timeout: 20 * 60 * 1000 });
run("verify:m9-13（P0/P1 映射/回归/覆盖率/平台）", pnpm.command, [...pnpm.prefix, "verify:m9-13"], { timeout: 20 * 60 * 1000 });
run("verify:m9-14（M9 G8 保持）", pnpm.command, [...pnpm.prefix, "verify:m9-14"], { timeout: 20 * 60 * 1000 });
run("verify:m10-03（HTTP/WS/CLI 发布门）", pnpm.command, [...pnpm.prefix, "verify:m10-03"], { timeout: 30 * 60 * 1000 });
run("verify:api-schema（OpenAPI diff=0）", pnpm.command, [...pnpm.prefix, "verify:api-schema"], { timeout: 20 * 60 * 1000 });
run("外部消费者（Node fetch/WS）", process.execPath, ["scripts/test/m10-03/external-client.mjs"], { timeout: 10 * 60 * 1000 });
run("health v2 旧 v1/新 v2 回归", process.execPath, ["scripts/test/m10-13/health-contract-regression.mjs"], { timeout: 10 * 60 * 1000 });

if (runGates) {
  run("verify:m10-09", pnpm.command, [...pnpm.prefix, "verify:m10-09"], { timeout: 60 * 60 * 1000 });
  run("verify:m10-10（ADR-039 基线）", pnpm.command, [...pnpm.prefix, "verify:m10-10"], { timeout: 20 * 60 * 1000 });
  run("verify:m10-11", pnpm.command, [...pnpm.prefix, "verify:m10-11"], { timeout: 60 * 60 * 1000 });
  run("verify:m10-14", pnpm.command, [...pnpm.prefix, "verify:m10-14"], { timeout: 60 * 60 * 1000 });
  run("verify:m10-14:level1", pnpm.command, [...pnpm.prefix, "verify:m10-14:level1"], { timeout: 30 * 60 * 1000 });
  run("verify:m10-14:level2", pnpm.command, [...pnpm.prefix, "verify:m10-14:level2"], { timeout: 30 * 60 * 1000 });
} else {
  console.log("NOTE  未传 --run-gates：跳过 m10-09/10/11/14 门禁串行复跑（证据已单独归档）");
}

const passed = checks.filter((item) => item.ok).length;
console.log(`\n[m10-13] 检查=${checks.length} 通过=${passed} 失败=${failures.length}`);
if (failures.length > 0) {
  console.error("[m10-13] FAIL：");
  for (const name of failures) console.error(` - ${name}`);
  process.exit(1);
}
console.log("AETHER_M10_13 PASS");
