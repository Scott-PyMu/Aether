#!/usr/bin/env node
/**
 * M10-10 验证入口（ADR-040 §4.1.1 / §4.1.2：ADR-039 基线模式）。
 *
 * 只校验 `scripts/test/m10-10/adr039-baseline.json` + `.sha256` 的来源/哈希/断言，
 * **不**断言当前实现仍是 ADR-039 全量口径（当前实现由 `verify:m10-14` 验证）。
 *
 * 检查项：
 *   ① JSON 自身 SHA-256 与 `.sha256` 一致；
 *   ② `source_commit` 固定为 `11cb438`（git 可用时校验 commit 存在）；
 *   ③ 6 个基线文件的 sha256/required_snippets 与固定提交内容一致；
 *   ④ 基线包含 ADR-039 的关键断言；
 *   ⑤ 历史证据文件存在。
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let failures = 0;
const check = (label, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? `：${detail}` : ""}`);
  if (!ok) failures += 1;
};
const read = (relative) => readFileSync(path.join(root, relative), "utf8");
const json = (relative) => JSON.parse(read(relative));
const exists = (relative) => existsSync(path.join(root, relative));

const baselinePath = "scripts/test/m10-10/adr039-baseline.json";
const hashPath = "scripts/test/m10-10/adr039-baseline.sha256";
check("基线 JSON 存在", exists(baselinePath));
check("基线 SHA-256 文件存在", exists(hashPath));
check("历史证据存在 docs/M10-10-证据.md", exists("docs/M10-10-证据.md"));
check("历史指标存在 docs/M10-10-metrics.json", exists("docs/M10-10-metrics.json"));

const raw = read(baselinePath);
const recorded = read(hashPath).trim().split(/\s+/)[0];
const actual = createHash("sha256").update(raw, "utf8").digest("hex");
check("基线 JSON SHA-256 匹配", recorded === actual, `${recorded} vs ${actual}`);

const baseline = JSON.parse(raw);
check("基线 task=M10-10", baseline.task === "M10-10");
check("基线 schema_version=1", baseline.schema_version === 1);
check("基线 ADR 标注", String(baseline.adr ?? "").includes("ADR-039"));
const sourceShort = String(baseline.source_short ?? "");
const sourceCommit = String(baseline.source_commit ?? "");
check("source_short=11cb438", sourceShort === "11cb438", sourceShort);
check("source_commit 为完整哈希", /^[0-9a-f]{40}$/.test(sourceCommit), sourceCommit);

let gitAvailable = false;
try {
  const resolved = execFileSync("git", ["rev-parse", sourceShort], {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  }).trim();
  gitAvailable = true;
  check("固定提交存在且哈希一致", resolved === sourceCommit, `${resolved}`);
} catch {
  console.log("NOTE  git 不可用：仅校验记录哈希/断言，不重算固定提交内容");
}

const expectedFiles = [
  "crates/aether-store/src/layout.rs",
  "crates/aether-store/src/write_queue.rs",
  "crates/aether-app/src/core_health.rs",
  "scripts/test/m10-10/verify-m10-10.mjs",
  "apps/desktop/electron/app.ts",
  "apps/desktop/src/m10_10_storage_check.test.tsx",
];
const files = Array.isArray(baseline.files) ? baseline.files : [];
check("基线文件数=6", files.length === 6, `${files.length}`);
check(
  "基线文件集合完整",
  expectedFiles.every((expected) => files.some((entry) => entry.path === expected)),
);

for (const entry of files) {
  check(
    `${entry.path} sha256 形状`,
    /^[0-9a-f]{64}$/.test(String(entry.sha256 ?? "")),
  );
  check(
    `${entry.path} required_snippets 非空`,
    Array.isArray(entry.required_snippets) && entry.required_snippets.length > 0,
  );
  if (!gitAvailable) continue;
  const content = execFileSync("git", ["show", `${sourceShort}:${entry.path}`], {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 20 * 1024 * 1024,
  });
  const digest = createHash("sha256").update(content, "utf8").digest("hex");
  check(`${entry.path} 固定提交内容哈希一致`, digest === entry.sha256);
  for (const snippet of entry.required_snippets ?? []) {
    check(`${entry.path} 片段存在`, content.includes(snippet), snippet);
  }
}

const layoutEntry = files.find((entry) => entry.path === "crates/aether-store/src/layout.rs");
check(
  "ADR-039 基线断言 full_startup_plan",
  (layoutEntry?.required_snippets ?? []).some((snippet) => snippet.includes("full_startup_plan")),
);
const verifierEntry = files.find(
  (entry) => entry.path === "scripts/test/m10-10/verify-m10-10.mjs",
);
check(
  "ADR-039 基线断言 !dirty_shards",
  (verifierEntry?.required_snippets ?? []).some((snippet) => snippet.includes("dirty_shards")),
);

const metrics = json("docs/M10-10-metrics.json");
check("历史 metrics task=M10-10", metrics.task === "M10-10");
check("历史 metrics 保留 storageCheckCompletedMs", metrics.electron?.storageCheckCompletedMs !== undefined);

if (failures > 0) {
  console.error(`[m10-10] FAIL（${failures} 项；ADR-039 基线模式）`);
  process.exit(1);
}
console.log("AETHER_M10_10 PASS（ADR-039 基线模式）");
