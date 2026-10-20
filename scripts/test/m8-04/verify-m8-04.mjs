#!/usr/bin/env node
/**
 * M8-04 验证入口：P1 发布与迁移。
 *
 * 覆盖：
 * - DoD1：P0 0003 → P1 0008 迁移演练（旧数据零丢失、FTS rebuild、幂等、备份候选）；
 * - DoD2：安装产物（平台构建；CI 三平台；本机缺产物时显式 SKIP）；
 * - DoD3：发布/恢复文档 P1 口径（升级路径、DSH pin、干净机冒烟）；
 * - DoD4：诊断包脱敏扫描 0 命中 + SBOM/产物哈希归档。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const skip = (name, reason) => checks.push({ name, skip: true, reason });

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m8-04", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });
const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
const assert = (condition, message) => {
  if (!condition) throw new Error(message);
};

// ===== 1. 静态：迁移/文档/SBOM 入口 =====
try {
  assert(existsSync(path.join(repoRoot, "crates/aether-store/tests/m8_04_migration.rs")), "缺少迁移演练测试");
  const release = read("docs/打包与发布说明.md");
  for (const token of ["0008", "0003", "0.2.1-rc.2", "P1 干净机冒烟", "generate-sbom.mjs"]) {
    assert(release.includes(token), `发布说明缺「${token}」`);
  }
  const recovery = read("docs/恢复手册.md");
  assert(recovery.includes("P1"), "恢复手册未覆盖 P1");
  assert(existsSync(path.join(repoRoot, "docs/M8-04-证据.md")), "缺少 docs/M8-04-证据.md");
  const packageJson = JSON.parse(read("package.json"));
  assert(packageJson.scripts?.["verify:m8-04"] === "node scripts/test/m8-04/verify-m8-04.mjs", "package.json 缺 verify:m8-04");
  record("静态：迁移测试/发布与恢复文档 P1 口径/SBOM 入口/证据登记", 0);
} catch (failure) {
  record(`静态检查失败：${failure && failure.message ? failure.message : failure}`, 1);
}

// ===== 2. DoD1：迁移演练（两次，验证可重复） =====
record(
  "DoD1 迁移演练第 1 次（P0 0003 → P1 0008；旧数据/FTS/幂等/备份候选）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m8_04_migration"], { cwd: repoRoot }),
);
record(
  "DoD1 迁移演练第 2 次（可重复执行）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m8_04_migration"], { cwd: repoRoot }),
);

// ===== 3. DoD4：脱敏扫描 =====
record(
  "DoD4 脱敏扫描（aether-security redaction_scan）",
  run(cargo, ["test", "-p", "aether-security", "--test", "redaction_scan"], { cwd: repoRoot }),
);
record(
  "DoD4 诊断包脱敏（m3_05_diagnostics export_bundle_redacts_secrets…）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "m3_05_diagnostics",
      "export_bundle_redacts_secrets_and_includes_log_aggregation",
    ],
    { cwd: repoRoot },
  ),
);

// ===== 4. DoD4：SBOM + 产物哈希 =====
record(
  "DoD4 SBOM 清单 + 产物 SHA256 归档",
  run(process.execPath, [path.join(repoRoot, "scripts/test/m8-04/generate-sbom.mjs")], {
    cwd: repoRoot,
  }),
);
const sbomRoot = path.join(repoRoot, "scripts", "test", ".tmp", "m8-04");
const latestSbom = existsSync(sbomRoot)
  ? readdirSync(sbomRoot, { withFileTypes: true })
      .filter((entry) => entry.isDirectory() && entry.name.startsWith("sbom-"))
      .map((entry) => entry.name)
      .sort()
      .at(-1)
  : null;
if (latestSbom) {
  const dir = path.join(sbomRoot, latestSbom);
  const sbomOk =
    existsSync(path.join(dir, "sbom.json")) && existsSync(path.join(dir, "SHA256SUMS.txt"));
  record(`DoD4 SBOM 归档核验（${latestSbom}）`, sbomOk ? 0 : 1);
} else {
  record("DoD4 SBOM 归档核验（未发现 sbom-* 目录）", 1);
}

// ===== 5. DoD2：安装产物（本机平台；CI 三平台）=====
const msiDir = path.join(repoRoot, "dist-out", "m8-04");
const msi = existsSync(msiDir)
  ? readdirSync(msiDir).filter((name) => /\.(msi|dmg)$/i.test(name))
  : [];
if (msi.length > 0) {
  record(`DoD2 本机安装产物（${msi.join(", ")}）`, 0);
} else {
  skip(
    "DoD2 本机安装产物（MSI/DMG）",
    "本机未产出 dist-out/m8-04/*.msi；三平台产物由 release-desktop-installers.yml 产出",
  );
}

const summary = {
  task: "M8-04",
  generated_at: new Date().toISOString(),
  evidence_dir: path.relative(repoRoot, evidenceDir),
  checks,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m8-04] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m8-04", checks));
