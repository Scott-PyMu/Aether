/**
 * 本地 CI 聚合入口（与 .github/workflows/ci.yml 的 M9/G8 检查步骤等价，便于本地复现）。
 *
 * M9-14/G8 后：Tauri 已退役，不再构建/测试 `aether-tauri`；默认门禁为
 * Electron + `aetherd` 架构。历史 P0/P1 验证脚本仍保留在 `scripts/test/` 供追溯，
 * 但其 Tauri 宿主已按 M9-13 删除清单退役，不再作为默认本地 CI 入口。
 *
 * 用法：node scripts/test/ci-local.mjs [--quick]
 *   --quick：跳过安装器冒烟与 M9-13 证据型门禁（仅做编译/lint/测试/build/静态门禁）。
 */
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run, summarize } from "./lib/exec.mjs";

const quick = process.argv.includes("--quick");
const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);
const script = (...parts) => path.join(repoRoot, "scripts", ...parts);

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

record("cargo fmt --check", run(cargo, ["fmt", "--all", "--", "--check"]));
record(
  "cargo clippy --workspace --all-targets -- -D warnings",
  run(cargo, ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
);
record("cargo check --workspace", run(cargo, ["check", "--workspace"]));
// M9-14/G8：Tauri 已退役；全 workspace 即最终发布面。
record("cargo test --workspace", run(cargo, ["test", "--workspace", "-j", "1", "--", "--test-threads=1"]));
record("pnpm version:check", run(node, [script("ci", "version.mjs"), "check"]));
record("pnpm typecheck", pnpmRun(["-r", "--if-present", "typecheck"]));
record("pnpm test", pnpmRun(["-r", "--if-present", "test"]));
record("pnpm build", pnpmRun(["-r", "--if-present", "build"]));
record("相似度扫描", run(node, [script("ci", "similarity-scan.mjs"), "--root", "."]));
record("npm 许可证检查", run(node, [script("ci", "check-npm-licenses.mjs")]));
record("cargo deny check", run(cargo, ["deny", "check"]));
record("verify-version-sync", run(node, [script("test", "verify-version-sync.mjs")]));
record("protocol generate/check", pnpmRun(["--filter", "@aether/protocol", "check"]));
record("verify:error-codes", pnpmRun(["verify:error-codes"]));
record("verify:m9-01-baseline", pnpmRun(["verify:m9-01-baseline"]));
record("verify:security（M1-08 → M9-10）", pnpmRun(["verify:security"]));
record("verify:m9-09（生命周期/数据锁）", pnpmRun(["verify:m9-09"]));
record("verify:m9-12（Electron/CI/E2E/覆盖率口径）", pnpmRun(["verify:m9-12"]));
record("verify:m9-14（G8 切换/退役）", pnpmRun(["verify:m9-14"]));
record("m9-14:failure-drill（G4–G7 失败处理）", pnpmRun(["m9-14:failure-drill"]));

if (!quick) {
  record("verify:m9-13（G7 证据门禁）", pnpmRun(["verify:m9-13"]));
  record("verify-coverage-gate", run(node, [script("test", "verify-coverage-gate.mjs")]));
  record("smoke:installer", pnpmRun(["smoke:installer"]));
}

process.exit(summarize("ci-local", checks));
