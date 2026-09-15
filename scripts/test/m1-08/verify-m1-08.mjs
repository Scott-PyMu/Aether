/**
 * M1-08 验证入口（Tauri 壳与安全基线）。
 *
 * 覆盖：
 *   DoD1/2 E2E：真实 WebView 中 CSP 阻断 + 外链导航拦截 + withGlobalTauri:false
 *   DoD3     单测矩阵：畸形参数（超长/未知字段/非法枚举/路径）→ 结构化错误且不落库
 *   DoD4     静态检查：capabilities 最小 allowlist、devtools 仅 debug（cargo tree）
 *
 * 用法：node scripts/test/m1-08/verify-m1-08.mjs [--skip-e2e] [--skip-tests]
 */
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "./env.mjs";

const args = process.argv.slice(2);
const skipE2e = args.includes("--skip-e2e");
const skipTests = args.includes("--skip-tests");

const env = buildEnv();
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

if (!skipTests) {
  record(
    "Rust 单测矩阵：cargo test -p aether-tauri（DoD3 + 导航 + 基线 + shell 冒烟）",
    run(bin("cargo"), ["test", "-p", "aether-tauri"], { env }),
  );
}

{
  // DoD4：devtools 仅 debug 可达——生产依赖树不得出现 devtools feature。
  const result = spawnSync(
    bin("cargo"),
    ["tree", "-p", "aether-tauri", "-e", "features"],
    { cwd: repoRoot, env, encoding: "utf8" },
  );
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  const matches = output
    .split(/\r?\n/)
    .filter((line) => line.includes("devtools"));
  if (result.status !== 0) {
    console.error(output);
  }
  if (matches.length > 0) console.error(matches.join("\n"));
  record(
    "devtools feature 未启用（cargo tree -e features）",
    result.status === 0 && matches.length === 0 ? 0 : 1,
  );
}

if (!skipE2e) {
  record(
    "CSP / 外链导航 E2E（真实 WebView2）",
    run(process.execPath, [path.join(repoRoot, "scripts", "test", "m1-08", "e2e-csp-navigation.mjs")], {
      env,
    }),
  );
}

process.exit(summarize("verify-m1-08", checks));
