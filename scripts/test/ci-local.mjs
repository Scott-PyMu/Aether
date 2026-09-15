/**
 * 本地 CI 聚合入口（与 .github/workflows/ci.yml 的检查步骤等价，便于本地复现）。
 * 用法：node scripts/test/ci-local.mjs [--quick]
 *   --quick：跳过覆盖率与冒烟（仅做格式/lint/测试/构建/门禁检查）。
 */
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run, summarize } from "./lib/exec.mjs";

const quick = process.argv.includes("--quick");
const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

record("cargo fmt --check", run(cargo, ["fmt", "--all", "--", "--check"]));
record(
  "cargo clippy --workspace -- -D warnings",
  run(cargo, ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
);
record("cargo check --workspace", run(cargo, ["check", "--workspace"]));
// 壳层 aether-tauri 的单元测试在 windows-gnu 下因 WebView2Loader/运行时差异无法运行；
// 壳层验证为构建+冒烟（smoke-desktop），覆盖率门禁同样排除该 crate。
record("cargo test --workspace（排除壳层）", run(cargo, ["test", "--workspace", "--exclude", "aether-tauri"]));
record("pnpm version:check", run(node, [path.join(repoRoot, "scripts", "ci", "version.mjs"), "check"]));
record("pnpm typecheck", pnpmRun(["-r", "--if-present", "typecheck"]));
record("pnpm test", pnpmRun(["-r", "--if-present", "test"]));
record("pnpm build", pnpmRun(["-r", "--if-present", "build"]));
record("相似度扫描", run(node, [path.join(repoRoot, "scripts", "ci", "similarity-scan.mjs"), "--root", "."]));
record("npm 许可证检查", run(node, [path.join(repoRoot, "scripts", "ci", "check-npm-licenses.mjs")]));
record("cargo deny check licenses/bans/sources", run(cargo, ["deny", "check", "licenses", "bans", "sources"]));
record("verify-version-sync", run(node, [path.join(repoRoot, "scripts", "test", "verify-version-sync.mjs")]));
record("verify-m1-02", run(node, [path.join(repoRoot, "scripts", "test", "m1-02", "verify-m1-02.mjs")]));
record("verify-m1-07（密钥自检/脱敏/降级演练）", run(node, [path.join(repoRoot, "scripts", "test", "m1-07-verify.mjs")]));
record(
  "verify-m1-08（壳与安全基线单测矩阵/静态检查；E2E 需真实 WebView，见 Windows CI）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-08", "verify-m1-08.mjs"), "--skip-e2e"]),
);

if (!quick) {
  record("verify-coverage-gate", run(node, [path.join(repoRoot, "scripts", "test", "verify-coverage-gate.mjs")]));
  record("smoke-desktop", run(node, [path.join(repoRoot, "scripts", "test", "smoke-desktop.mjs")]));
}

process.exit(summarize("ci-local", checks));
