/**
 * M4-06 验证入口：文档与安全收尾（实施计划 v1.24 §5 M4-06）。
 *
 * 覆盖 DoD：
 *   1) 文档齐备：README、适配器接入指南、失败场景处置手册（30 场景→操作步骤）、隐私说明；
 *   2) 新成员演练：按文档在干净环境接入 Mock 适配器并跑通（记录时长）；
 *   3) SBOM 归档；IPC 参数校验全量复查清单（36 命令）签核；密钥脱敏扫描 0 命中。
 *
 * 用法：node scripts/test/m4-06/verify-m4-06.mjs
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot, run, summarize } from "../lib/exec.mjs";
import { stampNow } from "../m4/lib/drill.mjs";

const checks = [];
const record = (name, ok, detail) =>
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
const evidenceDir = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m4-06",
  `evidence-${stampNow()}`,
);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. 文档齐备 =====

{
  const required = [
    ["README.md", "README"],
    ["docs/适配器接入指南.md", "适配器接入指南"],
    ["docs/失败场景处置手册.md", "失败场景处置手册"],
    ["docs/隐私说明.md", "隐私说明"],
    ["docs/打包与发布说明.md", "打包与发布说明"],
  ];
  const missing = required.filter(([file]) => !existsSync(path.join(repoRoot, file)));
  record(
    "文档齐备（README/接入指南/处置手册/隐私说明/发布说明）",
    missing.length === 0,
    missing.map(([, label]) => label).join(",") || "5/5",
  );

  const handbook = readFileSync(
    path.join(repoRoot, "docs", "失败场景处置手册.md"),
    "utf8",
  );
  const scenarioRows = handbook
    .split(/\r?\n/)
    .filter((line) => /^\|\s*\d+\s*\|/.test(line));
  record(
    "处置手册覆盖 30/30 失败场景（操作步骤逐条）",
    scenarioRows.length === 30,
    `场景行=${scenarioRows.length}`,
  );
}

// ===== 2. 新成员演练（文档可执行性 + 接入路径）=====

{
  const result = run(process.execPath, [
    path.join(repoRoot, "scripts", "test", "m4-06", "new-member-drill.mjs"),
  ]);
  record("新成员演练：按接入指南编译 Mock → hello → 会话闭环 → dispose/shutdown", result === 0);
}

// ===== 3. SBOM 归档（CI syft 产物 + 本地依赖清单归档）=====

{
  const ciWorkflow = readFileSync(
    path.join(repoRoot, ".github", "workflows", "ci.yml"),
    "utf8",
  );
  const syftOk = ciWorkflow.includes("syft dir:.") && ciWorkflow.includes("sbom-spdx");
  record("SBOM：CI 生成 SPDX JSON 并上传 artifact（syft/spdx）", syftOk);

  const cargoLock = readFileSync(path.join(repoRoot, "Cargo.lock"), "utf8");
  const pnpmLock = readFileSync(path.join(repoRoot, "pnpm-lock.yaml"), "utf8");
  const crates = [...cargoLock.matchAll(/^name = "([^"]+)"$/gm)].map((match) => match[1]);
  // pnpm-lock v9：包条目位于 `packages:` / `snapshots:` 段，形如
  // `  '@scope/pkg@1.2.3':` 或 `  pkg@1.2.3:`（不再带 v6 的前导 `/`）。
  const packages = [...pnpmLock.matchAll(/^  '?([^'\s][^:']*@[^':]+)'?:$/gm)].map(
    (match) => match[1],
  );
  const inventory = {
    generated_at: new Date().toISOString(),
    cargo_lock_sha256: createHash("sha256").update(cargoLock).digest("hex"),
    pnpm_lock_sha256: createHash("sha256").update(pnpmLock).digest("hex"),
    rust_crate_count: crates.length,
    npm_package_count: packages.length,
    rust_crates: [...new Set(crates)].sort(),
    npm_packages: [...new Set(packages)].sort(),
    canonical_sbom: "CI supply-chain job artifact `sbom-spdx`（syft dir:. spdx-json）",
  };
  const archiveDir = path.join(repoRoot, "docs", "evidence", "m4-06");
  mkdirSync(archiveDir, { recursive: true });
  writeFileSync(
    path.join(archiveDir, "dependency-inventory.json"),
    `${JSON.stringify(inventory, null, 2)}\n`,
    "utf8",
  );
  // 同一份副本进入本次证据目录（哈希一致性）。
  writeFileSync(
    path.join(evidenceDir, "dependency-inventory.json"),
    `${JSON.stringify(inventory, null, 2)}\n`,
    "utf8",
  );
  record(
    "SBOM 归档：依赖清单（Cargo.lock + pnpm-lock 摘要）落盘 docs/evidence/m4-06",
    crates.length > 0 && packages.length > 0,
    `crates=${crates.length} npm=${packages.length}`,
  );
}

// ===== 4. IPC 参数校验全量复查清单（36 命令）=====

{
  const checklistPath = path.join(repoRoot, "docs", "M4-06-IPC参数校验复查清单.md");
  const checklist = existsSync(checklistPath) ? readFileSync(checklistPath, "utf8") : "";
  const bindings = readFileSync(
    path.join(repoRoot, "packages", "protocol", "src", "bindings.ts"),
    "utf8",
  );
  const commands = [...bindings.matchAll(/__TAURI_INVOKE\("([a-z_]+)"/g)].map(
    (match) => match[1],
  );
  const uncovered = commands.filter((command) => !checklist.includes(`\`${command}\``));
  record(
    "IPC 参数校验全量复查清单：36/36 命令逐条登记",
    commands.length === 36 && uncovered.length === 0,
    uncovered.length === 0 ? `36/36` : `缺失=${uncovered.join(",")}`,
  );
  record(
    "错误码全集与警告码子表在清单登记（20 + thinking_depth_unsupported）",
    checklist.includes("错误码全集（20）") && checklist.includes("thinking_depth_unsupported"),
  );
}

// ===== 5. 密钥脱敏扫描 0 命中 =====

{
  const result = run(process.execPath, [path.join(repoRoot, "scripts", "test", "m1-07-verify.mjs")]);
  record("密钥脱敏扫描（sk-/JWT/PEM 三类样本日志与诊断导出 0 命中）", result === 0);
}

// ===== 归档 =====

writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(
    { task: "M4-06", stamp: path.basename(evidenceDir), checks },
    null,
    2,
  )}\n`,
  "utf8",
);
console.log(`[m4-06] 证据目录：${evidenceDir}`);
process.exit(summarize("verify-m4-06", checks));
