#!/usr/bin/env node
/**
 * M4-05：安装器产物哈希清单（签名后刷新；DoD3「产物哈希记录」）。
 *
 * 读取 `dist-out/` 下的 .msi/.dmg 产物，输出 `SHA256SUMS.txt`（`<sha256>  <file>` 行）。
 * 用法：node scripts/ci/hash-installers.mjs [--dir <目录>]
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot } from "../test/lib/exec.mjs";

const index = process.argv.indexOf("--dir");
const outDir = path.resolve(
  repoRoot,
  index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : "dist-out",
);
if (!existsSync(outDir)) {
  console.error(`[hash-installers] 目录不存在：${outDir}`);
  process.exit(1);
}
const files = readdirSync(outDir)
  .filter((name) => name.endsWith(".msi") || name.endsWith(".dmg"))
  .sort();
if (files.length === 0) {
  console.error(`[hash-installers] 未找到 .msi/.dmg 产物：${outDir}`);
  process.exit(1);
}
const lines = files.map((file) => {
  const hash = createHash("sha256").update(readFileSync(path.join(outDir, file))).digest("hex");
  return `${hash}  ${file}`;
});
writeFileSync(path.join(outDir, "SHA256SUMS.txt"), `${lines.join("\n")}\n`, "utf8");
for (const line of lines) console.log(`[hash-installers] ${line}`);
