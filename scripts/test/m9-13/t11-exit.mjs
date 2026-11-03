#!/usr/bin/env node
/**
 * M9-13 T11 映射：退出可靠性（≤10s，无孤儿）。
 *
 * `pnpm e2e:m9-09` 默认同时跑 UI 启动时序（T1）与退出场景；T11 只关心
 * checkpoint / write-queue / stuck 三条退出路径。本包装逐场景设置
 * `AETHER_M9_09_ONLY`，避免把 T1 的 debug 启动时序抖动误记为 T11 失败。
 *
 * 另跑 `cargo test -p aether-daemon --test m9_09_lifecycle`（stdin EOF/锁/8.2s 预算）。
 */
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { pnpmCommand, repoRoot } from "../lib/exec.mjs";

const tmpDir = path.join(repoRoot, "dist-out", "tmp");
const pnpm = pnpmCommand();
const results = [];

function run(name, command, args, env = {}) {
  console.log(`\n===== ${name} =====`);
  console.log(`$ ${[command, ...args].join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    encoding: "utf8",
    env: {
      ...process.env,
      ...env,
      TEMP: tmpDir,
      TMP: tmpDir,
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
    },
    timeout: 10 * 60 * 1000,
    maxBuffer: 64 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  console.log(output.trim().split(/\r?\n/).slice(-20).join("\n"));
  const ok = result.status === 0;
  results.push({ name, ok, exit: result.status ?? 1 });
  return ok;
}

run("T11 daemon 生命周期（stdin EOF/锁/8.2s）", "cargo", [
  "test",
  "-p",
  "aether-daemon",
  "--test",
  "m9_09_lifecycle",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);

for (const scenario of ["checkpoint", "write-queue", "stuck"]) {
  run(
    `T11 退出场景：${scenario}`,
    pnpm.command,
    [...pnpm.prefix, "e2e:m9-09"],
    { AETHER_M9_09_ONLY: scenario },
  );
}

const passed = results.every((item) => item.ok);
console.log("");
for (const item of results) {
  console.log(`${item.ok ? "PASS" : "FAIL"}  ${item.name}  exit=${item.exit}`);
}
if (!passed) {
  console.error(`[m9-13-t11] FAIL ${results.filter((item) => !item.ok).length} 项`);
  process.exit(1);
}
console.log("[m9-13-t11] PASS");
console.log("AETHER_M9_13_T11 PASS");
