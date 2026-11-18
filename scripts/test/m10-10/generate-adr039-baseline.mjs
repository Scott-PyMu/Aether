#!/usr/bin/env node
/**
 * ADR-040 §4.1.1：从固定提交 `11cb438` 生成 ADR-039 历史基线（只读审计用）。
 *
 * 仅当基线需要更新（评审）时运行；CI 不得自动改写。
 * 生成：
 *   - `adr039-baseline.json`：source_commit/文件 sha256/required_snippets
 *   - `adr039-baseline.sha256`：JSON 文件的 SHA-256
 *
 * 用法：node scripts/test/m10-10/generate-adr039-baseline.mjs
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const sourceShort = "11cb438";
const sourceCommit = execFileSync("git", ["rev-parse", sourceShort], {
  cwd: root,
  encoding: "utf8",
}).trim();

const files = [
  {
    path: "crates/aether-store/src/layout.rs",
    required_snippets: ["fn full_startup_plan", 'full_startup_plan(layout, "fingerprint_mismatch")'],
  },
  {
    path: "crates/aether-store/src/write_queue.rs",
    required_snippets: ["background_check_completed", "if strict_start"],
  },
  {
    path: "crates/aether-app/src/core_health.rs",
    required_snippets: ["if plan.strict_ok", "spawn_periodic_full_check"],
  },
  {
    path: "scripts/test/m10-10/verify-m10-10.mjs",
    required_snippets: ['!layout.includes("dirty_shards")', "full_startup_plan"],
  },
  {
    path: "apps/desktop/electron/app.ts",
    required_snippets: ["storageCheckCompletedAt"],
  },
  {
    path: "apps/desktop/src/m10_10_storage_check.test.tsx",
    required_snippets: ["storage_check", "checked_shards"],
  },
].map((entry) => {
  const content = execFileSync("git", ["show", `${sourceShort}:${entry.path}`], {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 20 * 1024 * 1024,
  });
  for (const snippet of entry.required_snippets) {
    if (!content.includes(snippet)) {
      throw new Error(`基线提交 ${sourceShort} 的 ${entry.path} 缺少片段：${snippet}`);
    }
  }
  return {
    path: entry.path,
    sha256: createHash("sha256").update(content, "utf8").digest("hex"),
    required_snippets: entry.required_snippets,
  };
});

const baseline = {
  task: "M10-10",
  adr: "ADR-039 v1.0 (amended by ADR-040 v1.0)",
  schema_version: 1,
  source_short: sourceShort,
  source_commit: sourceCommit,
  generated_at: new Date().toISOString(),
  generator: "scripts/test/m10-10/generate-adr039-baseline.mjs",
  files,
};

const outDir = path.join(root, "scripts", "test", "m10-10");
const jsonPath = path.join(outDir, "adr039-baseline.json");
const json = `${JSON.stringify(baseline, null, 2)}\n`;
writeFileSync(jsonPath, json, "utf8");
const hash = createHash("sha256").update(json, "utf8").digest("hex");
writeFileSync(path.join(outDir, "adr039-baseline.sha256"), `${hash}  adr039-baseline.json\n`, "utf8");
console.log(`ADR-039 baseline generated from ${sourceCommit}`);
console.log(`files=${files.length} sha256=${hash}`);
