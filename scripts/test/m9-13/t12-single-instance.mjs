#!/usr/bin/env node
/**
 * M9-13 T12 映射：双开防护（M9-07 E2E 的单实例场景）。
 *
 * 只断言 M9-07 E2E 输出中的「第二实例退出 + 聚焦已有窗口 + daemon 不重复 spawn」
 * 记录；M9-07 其余 36 条场景仍由 M9-07 任务自身门禁负责，避免把无关场景的
 * CI 抖动误记为 T12 失败。
 */
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { pnpmCommand, repoRoot } from "../lib/exec.mjs";

const tmpDir = path.join(repoRoot, "dist-out", "tmp");
const pnpm = pnpmCommand();
const result = spawnSync(
  pnpm.command,
  [...pnpm.prefix, "e2e:m9-07"],
  {
    cwd: repoRoot,
    encoding: "utf8",
    env: {
      ...process.env,
      TEMP: tmpDir,
      TMP: tmpDir,
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
    },
    timeout: 10 * 60 * 1000,
    maxBuffer: 64 * 1024 * 1024,
  },
);
const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
const line = output
  .split(/\r?\n/)
  .find((item) => item.startsWith("PASS") && item.includes("DoD-3") && item.includes("secondExit"));
const ok =
  Boolean(line) &&
  line.includes('"secondExit":"exit:0"') &&
  line.includes('"daemonSpawnCount":1');
console.log(line ?? "未找到 M9-07 单实例 PASS 记录");
if (!ok) {
  console.error(output.trim().split(/\r?\n/).slice(-30).join("\n"));
  console.error("[m9-13-t12] FAIL 双开防护记录缺失或不符合预期");
  process.exit(1);
}
console.log("[m9-13-t12] PASS 双开防护（第二实例退出 + 聚焦 + daemon 不重复 spawn）");
console.log("AETHER_M9_13_T12 PASS");
