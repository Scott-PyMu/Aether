/**
 * 验证「覆盖率门禁生效」（M1-01 DoD3）。
 *
 * 正向：真实代码在阈值 70 下通过（Rust 与 TS）。
 * 反向：使用专用夹具（必然低于阈值）证明阈值确实阻断。
 *
 * Rust：cargo-llvm-cov --fail-under-lines 70（排除无业务逻辑的壳层 aether-tauri，
 *       壳层由 CI 的构建+冒烟验证；该排除为任务内常量级决策，见自检报告）。
 *       注意：Windows GNU 工具链未随发行版提供 profiler_builtins，无法使用
 *       -C instrument-coverage；此环境自动跳过 Rust 项（CI 在 ubuntu 执行完整检查）。
 * TS：vitest --coverage 阈值取自 AETHER_COVERAGE_LINES（默认 70）。
 */
import { cpSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run, rustcHost, summarize } from "./lib/exec.mjs";

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "coverage-gate");
const rustFixture = path.join(tmpDir, "uncovered-crate");
const tsFixtureSource = path.join(repoRoot, "scripts", "test", "fixtures", "coverage-fail-ts");
const tsFixtureWorkDir = path.join(repoRoot, "apps", "desktop", ".tmp-coverage-gate");
const rustCoverageUnsupported = rustcHost().endsWith("windows-gnu");

const checks = [];

function prepareRustFixture() {
  mkdirSync(path.join(rustFixture, "src"), { recursive: true });
  writeFileSync(
    path.join(rustFixture, "Cargo.toml"),
    [
      "[package]",
      'name = "uncovered-crate"',
      'version = "0.0.0"',
      'edition = "2021"',
      "",
      "[workspace]",
      "",
    ].join("\n"),
    "utf8",
  );
  writeFileSync(
    path.join(rustFixture, "src", "lib.rs"),
    [
      "pub fn fully_uncovered(input: u32) -> u32 {",
      "    let mut total = 0;",
      "    for index in 0..input {",
      "        total += index;",
      "    }",
      "    total + 1",
      "}",
      "",
    ].join("\n"),
    "utf8",
  );
}

function cleanup() {
  rmSync(tmpDir, { recursive: true, force: true });
  rmSync(tsFixtureWorkDir, { recursive: true, force: true });
}

cleanup();

if (rustCoverageUnsupported) {
  checks.push({
    name: "Rust：真实 workspace 覆盖率 >= 70（cargo llvm-cov）",
    skip: true,
    reason: "windows-gnu 工具链缺少 profiler_builtins，由 CI（ubuntu）执行",
  });
  checks.push({
    name: "Rust：夹具覆盖率 < 70 时必须阻断",
    skip: true,
    reason: "同上",
  });
} else {
  prepareRustFixture();
  checks.push({
    name: "Rust：真实 workspace 覆盖率 >= 70（cargo llvm-cov）",
    expect: 0,
    exit: run(cargo, [
      "llvm-cov",
      "--workspace",
      "--exclude",
      "aether-tauri",
      "--fail-under-lines",
      "70",
    ]),
  });
  checks.push({
    name: "Rust：夹具覆盖率 < 70 时必须阻断（非零）",
    expect: "nonzero",
    exit: run(cargo, [
      "llvm-cov",
      "--manifest-path",
      path.join(rustFixture, "Cargo.toml"),
      "--fail-under-lines",
      "70",
    ]),
  });
}

try {
  cpSync(tsFixtureSource, tsFixtureWorkDir, { recursive: true });
  checks.push({
    name: "TS：真实包覆盖率 >= 70（vitest --coverage，AETHER_COVERAGE_LINES=70）",
    expect: 0,
    exit: run(pnpm, [...pnpmPrefix, "-r", "--if-present", "coverage"], {
      env: { AETHER_COVERAGE_LINES: "70" },
    }),
  });
  checks.push({
    name: "TS：夹具覆盖率 < 阈值时必须阻断（非零）",
    expect: "nonzero",
    exit: run(
      pnpm,
      [
        ...pnpmPrefix,
        "exec",
        "vitest",
        "run",
        "--coverage",
        "--config",
        path.join(tsFixtureWorkDir, "vitest.config.ts"),
      ],
      { cwd: path.join(repoRoot, "apps", "desktop") },
    ),
  });
} finally {
  cleanup();
}

process.exit(summarize("verify-coverage-gate", checks));
