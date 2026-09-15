/**
 * 验证「版本号统一管理（单一来源）」门禁（M1-01）。
 *
 * 步骤：
 *   1) 仓库版本一致性检查：期望退出 0；
 *   2) 不一致夹具：期望退出 1（证明门禁可阻断）。
 */
import path from "node:path";
import process from "node:process";
import { repoRoot, run, summarize } from "./lib/exec.mjs";

const node = process.execPath;
const versionScript = path.join(repoRoot, "scripts", "ci", "version.mjs");
const fixtureRoot = path.join(repoRoot, "scripts", "test", "fixtures", "version-mismatch");

const checks = [];

checks.push({
  name: "仓库版本一致性（单一来源同步）",
  expect: 0,
  exit: run(node, [versionScript, "check"]),
});

checks.push({
  name: "版本不一致夹具（门禁阻断）",
  expect: 1,
  exit: run(node, [versionScript, "check", "--root", fixtureRoot]),
});

process.exit(summarize("verify-version-sync", checks));
