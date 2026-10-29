#!/usr/bin/env node
/**
 * 准备 M9-09 真实后端会话计时 E2E：构建含 Mock 的 runtime-bundle，并重建 aetherd
 * 使 build.rs 内嵌该清单摘要。
 *
 * 用法：
 *   pnpm prepare:m9-09:real-session
 *   pnpm e2e:m9-09:real-session
 *
 * 说明：`crates/aether-tauri/runtime-bundle` 与 `target` 均为本地产物；此脚本只改变
 * 构建产物，不修改源码/协议/命令面。恢复 official-only 构建：
 *   node scripts/ci/build-runtime-bundles.mjs --out crates/aether-tauri/runtime-bundle
 *   cargo build -p aether-daemon
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const bundleDir = path.join(repoRoot, "crates", "aether-tauri", "runtime-bundle");
const builder = path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs");

function run(command, args, env = process.env) {
  console.log(`$ ${command} ${args.join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    env,
    stdio: "inherit",
    windowsHide: true,
  });
  if (result.error) {
    console.error(`[m9-09-prepare] 启动失败：${result.error.message}`);
    process.exit(1);
  }
  if (result.status !== 0) {
    console.error(`[m9-09-prepare] 退出码 ${result.status}`);
    process.exit(result.status ?? 1);
  }
}

if (!existsSync(builder)) {
  console.error(`[m9-09-prepare] 缺少构建脚本：${builder}`);
  process.exit(1);
}

run(process.execPath, [builder, "--out", bundleDir], {
  ...process.env,
  AETHER_BUNDLE_MOCK: "1",
});
run("cargo", ["build", "-p", "aether-daemon"]);
console.log("[m9-09-prepare] 完成：Mock runtime-bundle + aetherd（含 Mock 摘要）");
console.log("[m9-09-prepare] 下一步：pnpm e2e:m9-09:real-session");
