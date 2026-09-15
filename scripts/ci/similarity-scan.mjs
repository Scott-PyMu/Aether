#!/usr/bin/env node
/**
 * 相似度/合规扫描（设计 A11 净室合规；AGENTS §2.10）。
 *
 * 模式一（CI 默认）：规则扫描。逐行匹配 scripts/ci/similarity-rules.json 的规则，
 *   命中即退出 1（阻断合并）。
 * 模式二（本地开发可选）：`--ref <dir>` 与参考语料做「归一化整行」比对，
 *   疑似逐行拷贝（>= --ref-threshold 行）即退出 1。CI 不依赖参考语料，仅本地使用。
 *
 * 用法：
 *   node scripts/ci/similarity-scan.mjs [--root <dir>] [--rules <path>] [--ref <dir>]
 *                                       [--ref-threshold <n>] [--json]
 *
 * 退出码：0 通过；1 命中；2 用法/配置错误。
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..", "..");
const defaultRules = path.join(scriptDir, "similarity-rules.json");

function parseArgs(argv) {
  const options = {
    root: repoRoot,
    rules: defaultRules,
    ref: null,
    refThreshold: 10,
    json: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = argv[i + 1];
    switch (arg) {
      case "--root":
        options.root = path.resolve(value);
        i += 1;
        break;
      case "--rules":
        options.rules = path.resolve(value);
        i += 1;
        break;
      case "--ref":
        options.ref = path.resolve(value);
        i += 1;
        break;
      case "--ref-threshold":
        options.refThreshold = Number(value);
        i += 1;
        break;
      case "--json":
        options.json = true;
        break;
      case "--help":
        console.log(
          "usage: similarity-scan.mjs [--root <dir>] [--rules <path>] [--ref <dir>] [--ref-threshold <n>] [--json]",
        );
        process.exit(0);
        break;
      default:
        console.error(`未知参数：${arg}`);
        process.exit(2);
    }
  }
  return options;
}

function loadRules(rulesPath) {
  const rules = JSON.parse(readFileSync(rulesPath, "utf8"));
  rules.compiled = rules.rules.map((rule) => ({
    ...rule,
    matchers: rule.patterns.map((pattern) => new RegExp(pattern)),
  }));
  return rules;
}

function shouldSkipDir(name, excludedNames) {
  return excludedNames.includes(name);
}

function collectFiles(root, rules) {
  const excludedPaths = new Set(
    (rules.excludePaths ?? []).map((p) => path.normalize(p).replace(/[\\/]+$/, "")),
  );
  const extensions = new Set(rules.scanExtensions ?? []);
  const files = [];

  function walk(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const absolute = path.join(dir, entry.name);
      const relative = path.relative(root, absolute);
      const normalizedRelative = path.normalize(relative);
      if (entry.isDirectory()) {
        if (shouldSkipDir(entry.name, rules.excludeNames ?? [])) continue;
        if (excludedPaths.has(normalizedRelative)) continue;
        walk(absolute);
        continue;
      }
      if (!entry.isFile()) continue;
      if (excludedPaths.has(normalizedRelative)) continue;
      if (!extensions.has(path.extname(entry.name))) continue;
      files.push({ absolute, relative });
    }
  }

  walk(root);
  return files;
}

function scanRules(files, rules) {
  const hits = [];
  for (const file of files) {
    let text;
    try {
      text = readFileSync(file.absolute, "utf8");
    } catch {
      continue;
    }
    const lines = text.split(/\r?\n/);
    lines.forEach((line, index) => {
      for (const rule of rules.compiled) {
        const matched = rule.matchers.some((matcher) => matcher.test(line));
        if (!matched) continue;
        const exempt = (rule.exceptions ?? []).some((exception) => line.includes(exception));
        if (exempt) continue;
        hits.push({
          rule: rule.id,
          reason: rule.reason,
          file: file.relative,
          line: index + 1,
          snippet: line.trim().slice(0, 160),
        });
      }
    });
  }
  return hits;
}

function normalizeLine(line) {
  return line
    .replace(/\/\/.*$/, "")
    .replace(/#.*$/, "")
    .replace(/\s+/g, " ")
    .trim()
    .toLowerCase();
}

function collectNormalizedLines(root, rules) {
  const map = new Map();
  for (const file of collectFiles(root, rules)) {
    let text;
    try {
      text = readFileSync(file.absolute, "utf8");
    } catch {
      continue;
    }
    const set = new Set();
    for (const line of text.split(/\r?\n/)) {
      const normalized = normalizeLine(line);
      if (normalized.length >= 40) set.add(normalized);
    }
    if (set.size > 0) map.set(file.relative, set);
  }
  return map;
}

function scanReference(rootMap, refRoot, rules, threshold) {
  const refMap = collectNormalizedLines(refRoot, rules);
  const hits = [];
  for (const [repoFile, repoLines] of rootMap) {
    for (const [refFile, refLines] of refMap) {
      let shared = 0;
      for (const line of repoLines) {
        if (refLines.has(line)) shared += 1;
      }
      if (shared >= threshold) {
        hits.push({ file: repoFile, reference: refFile, sharedLines: shared });
      }
    }
  }
  return hits.sort((a, b) => b.sharedLines - a.sharedLines);
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  let rules;
  try {
    rules = loadRules(options.rules);
  } catch (error) {
    console.error(`[similarity] 规则加载失败：${error.message}`);
    return 2;
  }
  if (!statSync(options.root).isDirectory()) {
    console.error(`[similarity] --root 不是目录：${options.root}`);
    return 2;
  }

  const files = collectFiles(options.root, rules);
  const ruleHits = scanRules(files, rules);
  let refHits = [];
  if (options.ref) {
    if (!statSync(options.ref).isDirectory()) {
      console.error(`[similarity] --ref 不是目录：${options.ref}`);
      return 2;
    }
    refHits = scanReference(collectNormalizedLines(options.root, rules), options.ref, rules, options.refThreshold);
  }

  if (options.json) {
    console.log(JSON.stringify({ scannedFiles: files.length, ruleHits, refHits }, null, 2));
  } else {
    console.log(`[similarity] 扫描 ${files.length} 个文件（规则 ${rules.rules.length} 条）`);
    for (const hit of ruleHits) {
      console.error(`[similarity] 命中 ${hit.rule} @ ${hit.file}:${hit.line}`);
      console.error(`            原因：${hit.reason}`);
      console.error(`            片段：${hit.snippet}`);
    }
    for (const hit of refHits) {
      console.error(
        `[similarity] 疑似拷贝：${hit.file} 与参考 ${hit.reference} 共享 ${hit.sharedLines} 行`,
      );
    }
    if (ruleHits.length === 0 && refHits.length === 0) {
      console.log("[similarity] 通过：无规则命中");
    }
  }

  return ruleHits.length > 0 || refHits.length > 0 ? 1 : 0;
}

process.exit(main());
