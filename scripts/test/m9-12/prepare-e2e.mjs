#!/usr/bin/env node
/**
 * M9-12 E2E 前置（Playwright Electron + 真实 daemon + Mock 运行时）。
 *
 * 1. 构建**隔离**的测试 runtime-bundle（`dist-out/m9-12-runtime-bundle/`，含 Mock；
 *    ADR-008：Mock 仅测试构建）；
 * 2. 以同一目录的清单/摘要编译 `aetherd --features test-hooks`（ADR-015 §2.2#2
 *    信任锚；ADR-029 §7/§8.2 P2-10）。
 *
 * 该脚本只写构建产物（`dist-out/`、`target/`），不修改源码/protocol/命令面。
 * 恢复普通构建：`cargo build -p aether-daemon`（不启用 feature）。
 *
 * 用法：
 *   node scripts/test/m9-12/prepare-e2e.mjs [--out <dir>]
 */
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run } from "../lib/exec.mjs";

const builder = path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs");
const bundleDir = path.resolve(
  repoRoot,
  process.argv.includes("--out")
    ? process.argv[process.argv.indexOf("--out") + 1]
    : path.join("dist-out", "m9-12-runtime-bundle"),
);

if (!existsSync(builder)) {
  console.error(`[m9-12-prepare] 缺少构建脚本：${builder}`);
  process.exit(1);
}

console.log(`[m9-12-prepare] runtime-bundle → ${bundleDir}`);
const bundleCode = run(process.execPath, [builder, "--out", bundleDir], {
  cwd: repoRoot,
  env: { ...process.env, AETHER_BUNDLE_MOCK: "1" },
});
if (bundleCode !== 0) {
  console.error(`[m9-12-prepare] runtime-bundle 构建失败（exit=${bundleCode}）`);
  process.exit(bundleCode);
}

const manifestPath = path.join(bundleDir, "runtimes.json");
if (!existsSync(manifestPath)) {
  console.error(`[m9-12-prepare] 缺少清单：${manifestPath}`);
  process.exit(1);
}

console.log("[m9-12-prepare] cargo build -p aether-daemon --features test-hooks");
const daemonCode = run(
  bin("cargo"),
  ["build", "-p", "aether-daemon", "--features", "test-hooks"],
  {
    cwd: repoRoot,
    env: {
      ...process.env,
      AETHER_RUNTIME_BUNDLE_DIR: bundleDir,
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
    },
  },
);
if (daemonCode !== 0) {
  console.error(`[m9-12-prepare] aetherd 构建失败（exit=${daemonCode}）`);
  process.exit(daemonCode);
}

const daemonPath = path.join(
  repoRoot,
  "target",
  "debug",
  process.platform === "win32" ? "aetherd.exe" : "aetherd",
);
if (!existsSync(daemonPath)) {
  console.error(`[m9-12-prepare] 未找到 daemon 产物：${daemonPath}`);
  process.exit(1);
}

console.log(`[m9-12-prepare] 完成：${path.relative(repoRoot, daemonPath)}（test-hooks）`);
console.log("[m9-12-prepare] 下一步：pnpm e2e:m9-12");
