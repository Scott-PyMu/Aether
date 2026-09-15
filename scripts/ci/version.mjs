#!/usr/bin/env node
/**
 * 版本一致性工具（M1-01：单一版本来源，构建时注入）。
 *
 * 单一来源：工作区 Cargo.toml 的 [workspace.package].version。
 * 派生目标：根/各包的 package.json、packages/protocol/src/version.ts、
 *           Cargo.toml [workspace.dependencies] 中的 aether-* 版本引用。
 *
 * 用法：
 *   node scripts/ci/version.mjs check [--root <dir>]   # 校验，不一致退出 1（CI 门禁）
 *   node scripts/ci/version.mjs sync  [--root <dir>]   # 同步派生目标
 *   node scripts/ci/version.mjs print [--root <dir>]   # 打印单一来源版本
 */
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..", "..");

const PACKAGE_JSON_TARGETS = [
  "package.json",
  "apps/desktop/package.json",
  "packages/protocol/package.json",
];
const GENERATED_VERSION_FILE = "packages/protocol/src/version.ts";
const GENERATED_HEADER =
  "// 由 scripts/ci/version.mjs 生成，禁止手改。\n" +
  "// 修改版本号请改工作区 Cargo.toml 的 [workspace.package].version，然后运行 `pnpm version:sync`。\n";

export function readSourceVersion(root) {
  const manifestPath = path.join(root, "Cargo.toml");
  if (!existsSync(manifestPath)) {
    throw new Error(`找不到 Cargo.toml：${manifestPath}`);
  }
  const lines = readFileSync(manifestPath, "utf8").split(/\r?\n/);
  let inWorkspacePackage = false;
  for (const line of lines) {
    const section = line.match(/^\s*\[([^\]]+)\]\s*$/);
    if (section) {
      inWorkspacePackage = section[1].trim() === "workspace.package";
      continue;
    }
    if (!inWorkspacePackage) continue;
    const version = line.match(/^\s*version\s*=\s*"([^"]+)"\s*$/);
    if (version) return version[1];
  }
  throw new Error("Cargo.toml 的 [workspace.package] 中未找到 version 字段");
}

function expectedGeneratedVersionFile(version) {
  return `${GENERATED_HEADER}export const APP_VERSION = ${JSON.stringify(version)};\n`;
}

function checkPackageJson(root, version, problems) {
  for (const relative of PACKAGE_JSON_TARGETS) {
    const filePath = path.join(root, relative);
    if (!existsSync(filePath)) {
      problems.push(`${relative}: 文件缺失`);
      continue;
    }
    let parsed;
    try {
      parsed = JSON.parse(readFileSync(filePath, "utf8"));
    } catch (error) {
      problems.push(`${relative}: JSON 解析失败（${error.message}）`);
      continue;
    }
    if (parsed.version !== version) {
      problems.push(`${relative}: version=${parsed.version}，期望 ${version}`);
    } else {
      console.log(`[version] OK  ${relative}`);
    }
  }
}

function checkGeneratedFile(root, version, problems) {
  const filePath = path.join(root, GENERATED_VERSION_FILE);
  if (!existsSync(filePath)) {
    problems.push(`${GENERATED_VERSION_FILE}: 文件缺失（运行 pnpm version:sync 生成）`);
    return;
  }
  const actual = readFileSync(filePath, "utf8");
  const expected = expectedGeneratedVersionFile(version);
  if (actual !== expected) {
    problems.push(`${GENERATED_VERSION_FILE}: 内容与单一来源不一致（运行 pnpm version:sync）`);
  } else {
    console.log(`[version] OK  ${GENERATED_VERSION_FILE}`);
  }
}

function checkWorkspaceDependencyVersions(root, version, problems) {
  const manifest = readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const pattern = /(aether-[\w-]+)\s*=\s*\{[^}]*version\s*=\s*"([^"]+)"/g;
  let match;
  let found = 0;
  while ((match = pattern.exec(manifest)) !== null) {
    found += 1;
    if (match[2] !== version) {
      problems.push(
        `Cargo.toml [workspace.dependencies].${match[1]}: version=${match[2]}，期望 ${version}`,
      );
    }
  }
  if (found === 0) {
    problems.push("Cargo.toml [workspace.dependencies] 中未找到 aether-* 版本引用");
  } else if (problems.length === 0) {
    console.log(`[version] OK  Cargo.toml [workspace.dependencies]（${found} 个引用）`);
  }
}

function runCheck(root) {
  const version = readSourceVersion(root);
  console.log(`[version] 单一来源 Cargo.toml [workspace.package].version = ${version}`);
  const problems = [];
  checkPackageJson(root, version, problems);
  checkGeneratedFile(root, version, problems);
  checkWorkspaceDependencyVersions(root, version, problems);
  if (problems.length > 0) {
    console.error("[version] 版本不一致：");
    for (const problem of problems) console.error(`  - ${problem}`);
    return 1;
  }
  console.log("[version] 全部一致（check 通过）");
  return 0;
}

function runSync(root) {
  const version = readSourceVersion(root);
  let changed = 0;
  for (const relative of PACKAGE_JSON_TARGETS) {
    const filePath = path.join(root, relative);
    if (!existsSync(filePath)) continue;
    const parsed = JSON.parse(readFileSync(filePath, "utf8"));
    if (parsed.version !== version) {
      parsed.version = version;
      writeFileSync(filePath, `${JSON.stringify(parsed, null, 2)}\n`, "utf8");
      console.log(`[version] 更新 ${relative} -> ${version}`);
      changed += 1;
    }
  }
  const generatedPath = path.join(root, GENERATED_VERSION_FILE);
  const expected = expectedGeneratedVersionFile(version);
  const current = existsSync(generatedPath) ? readFileSync(generatedPath, "utf8") : "";
  if (current !== expected) {
    writeFileSync(generatedPath, expected, "utf8");
    console.log(`[version] 更新 ${GENERATED_VERSION_FILE} -> ${version}`);
    changed += 1;
  }
  console.log(`[version] sync 完成（${changed} 个文件更新，来源 ${version}）`);
  return 0;
}

function main() {
  const args = process.argv.slice(2);
  const command = args[0] ?? "check";
  const rootIndex = args.indexOf("--root");
  const root = rootIndex >= 0 && args[rootIndex + 1] ? path.resolve(args[rootIndex + 1]) : repoRoot;

  try {
    switch (command) {
      case "check":
        return runCheck(root);
      case "sync":
        return runSync(root);
      case "print":
        console.log(readSourceVersion(root));
        return 0;
      default:
        console.error(`未知命令：${command}（可用：check / sync / print）`);
        return 2;
    }
  } catch (error) {
    console.error(`[version] 执行失败：${error.message}`);
    return 1;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main());
}
