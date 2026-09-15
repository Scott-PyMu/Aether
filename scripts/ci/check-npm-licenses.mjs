#!/usr/bin/env node
/**
 * npm 依赖许可证门禁（设计 §2.2 供应链：license-checker；AGENTS §2.10）。
 *
 * 读取 scripts/ci/npm-license-allowlist.json，扫描整个 pnpm workspace 的依赖，
 * 出现白名单外许可证（GPL/AGPL/BUSL 等）即退出 1。
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

function loadAllowlist() {
  const filePath = path.join(root, "scripts", "ci", "npm-license-allowlist.json");
  const parsed = JSON.parse(readFileSync(filePath, "utf8"));
  if (!Array.isArray(parsed.allow) || parsed.allow.length === 0) {
    throw new Error(`${filePath} 中 allow 列表为空`);
  }
  return parsed.allow;
}

function isAllowed(licenses, allowlist) {
  if (typeof licenses !== "string" || licenses.length === 0) return false;
  return allowlist.some((allowed) => licenses.includes(allowed));
}

function main() {
  let allowlist;
  try {
    allowlist = loadAllowlist();
  } catch (error) {
    console.error(`[licenses] 白名单加载失败：${error.message}`);
    return 2;
  }

  const licenseChecker = require("license-checker");
  return new Promise((resolve) => {
    licenseChecker.init(
      { start: root, production: false, excludePrivatePackages: true },
      (error, packages) => {
        if (error) {
          console.error(`[licenses] license-checker 失败：${error.message}`);
          resolve(1);
          return;
        }
        const entries = Object.entries(packages ?? {});
        const violations = entries.filter(([, info]) => !isAllowed(info.licenses, allowlist));
        console.log(`[licenses] 扫描 ${entries.length} 个依赖；白名单 ${allowlist.length} 项`);
        if (violations.length === 0) {
          console.log("[licenses] 通过：无白名单外许可证");
          resolve(0);
          return;
        }
        console.error(`[licenses] 阻断：${violations.length} 个依赖许可证不在白名单：`);
        for (const [name, info] of violations) {
          console.error(`  - ${name}: ${info.licenses} (${info.path ?? "未知路径"})`);
        }
        resolve(1);
      },
    );
  }).then((code) => process.exit(code));
}

main();
