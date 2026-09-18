/**
 * M1-08 验证入口（Tauri 壳与安全基线）。
 *
 * 覆盖：
 *   DoD1/2 E2E：真实 WebView 中 CSP 阻断 + 外链导航拦截 + withGlobalTauri:false
 *   DoD3     单测矩阵：畸形参数（超长/未知字段/非法枚举/路径）→ 结构化错误且不落库；
 *            ADR-004 七命令（backup_list/backup_restore/app_restart/run_retry/
 *            runtime_retry/runtime_enable/workspace_set）全部在列并注册；
 *            session_send.client_msg_id 必填 ULID（ADR-005 幂等键）
 *   DoD4     静态检查：capabilities 最小 allowlist、devtools 仅 debug（cargo tree）
 *
 * 用法：node scripts/test/m1-08/verify-m1-08.mjs [--skip-e2e] [--skip-tests]
 */
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
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

{
  // DoD3（ADR-004 + ADR-007）：命令必须同时存在于命令定义与 debug/release 两个 handler 注册列表。
  const source = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "ipc", "commands.rs"),
    "utf8",
  );
  const required = [
    "backup_list",
    "backup_restore",
    "app_restart",
    "run_retry",
    "runtime_retry",
    "runtime_enable",
    "workspace_set",
    // ADR-007 决策 1：health（无参数；真实数据接线归 M2-07）。
    "health",
  ];
  const problems = [];
  for (const command of required) {
    if (!source.includes(`fn ${command}(`)) problems.push(`${command}: 缺少命令定义`);
    const registrations = (source.match(new RegExp(`\\b${command}\\b`, "g")) ?? []).length;
    if (registrations < 3) {
      problems.push(`${command}: 注册次数 ${registrations} < 3（定义 + debug/release handler）`);
    }
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "ADR-004 七命令 + ADR-007 health 定义与注册",
    problems.length === 0 ? 0 : 1,
  );
}

{
  // ADR-007 增量 2：T12 顺序过渡窗口错误码 core_not_ready 在案；
  // health 真实返回测试（normal/persist_degraded/core_not_ready/wired-runtimes）已签入。
  const errorSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "ipc", "error.rs"),
    "utf8",
  );
  const healthTest = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "tests", "health_command.rs"),
    "utf8",
  );
  const problems = [];
  if (!/CoreNotReady/.test(errorSource) || !/"core_not_ready"/.test(errorSource)) {
    problems.push("error.rs 缺少 core_not_ready 错误码（ADR-007 增量 2）");
  }
  for (const test of [
    "core_not_ready_until_backend_installed",
    "health_passes_through_wired_runtime_summaries",
  ]) {
    if (!healthTest.includes(`fn ${test}(`)) problems.push(`health_command.rs 缺少用例 ${test}`);
  }
  if (!/install_backend/.test(readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "ipc", "mod.rs"),
    "utf8",
  ))) {
    problems.push("ipc/mod.rs 缺少延迟后端注入 install_backend（T12 顺序）");
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "ADR-007 增量 2：core_not_ready 与 runtimes 语义（T12 顺序测试在案）",
    problems.length === 0 ? 0 : 1,
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
