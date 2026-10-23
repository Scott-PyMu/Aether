#!/usr/bin/env node
/**
 * M7-10 / D10 密钥扫描（ADR-028 v0.9 §2.4）。
 *
 * 扫描对象：非测试源码、配置、迁移、CI 脚本（排除 tests/、*.test.*、node_modules、
 * target、runtime-bundle、.tmp、docs/ 描述性文本）。
 * 合成/假 key 仅允许出现在测试代码/夹具；本脚本对非测试源码要求 0 命中。
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot } from "../lib/exec.mjs";

const ROOTS = [
  "apps/desktop/src",
  "crates",
  "packages",
  "scripts/ci",
  "migrations",
  "Cargo.toml",
  "deny.toml",
  "package.json",
  "pnpm-workspace.yaml",
];

const EXCLUDED_DIRS = new Set([
  "node_modules",
  "target",
  "runtime-bundle",
  ".tmp",
  "dist",
  "dist-out",
  "acl-recovery",
]);
const EXCLUDED_FILE = /(\.test\.(ts|tsx|js)$)|(^|[/\\])tests([/\\]|$)/;
const ALLOWED_EXT = /\.(rs|ts|tsx|js|mjs|json|toml|ya?ml|sql|md)$/i;

const PATTERNS = [
  { name: "anthropic-key", regex: /sk-ant-[A-Za-z0-9_-]{8,}/ },
  { name: "openai-key", regex: /sk-(?:proj-)?[A-Za-z0-9]{20,}/ },
  { name: "jwt", regex: /eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/ },
  { name: "pem-private", regex: /-----BEGIN [A-Z ]*PRIVATE KEY-----/ },
];

const hits = [];

function scanFile(file) {
  if (EXCLUDED_FILE.test(file)) return;
  if (!ALLOWED_EXT.test(file)) return;
  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch {
    return;
  }
  for (const pattern of PATTERNS) {
    if (pattern.regex.test(text)) {
      hits.push(`${pattern.name}: ${path.relative(repoRoot, file)}`);
    }
  }
}

function walk(relative) {
  const absolute = path.join(repoRoot, relative);
  let stat;
  try {
    stat = statSync(absolute);
  } catch {
    return;
  }
  if (stat.isFile()) {
    scanFile(absolute);
    return;
  }
  for (const entry of readdirSync(absolute, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      if (EXCLUDED_DIRS.has(entry.name)) continue;
      walk(path.join(relative, entry.name));
    } else if (entry.isFile()) {
      scanFile(path.join(absolute, entry.name));
    }
  }
}

for (const root of ROOTS) walk(root);

if (hits.length > 0) {
  console.error("[m7-10] 密钥扫描命中（非测试源码）：");
  for (const hit of hits) console.error(`- ${hit}`);
  process.exit(1);
}
console.log("[m7-10] 密钥扫描：非测试源码/运行产物 0 命中（测试代码与合成 key 白名单除外）");
