#!/usr/bin/env node
/**
 * M9-13 DoD-1/DoD-2：全量回归 + P0/P1 映射复跑（可执行证据）。
 *
 * 运行清单（顺序执行；失败继续并记录，最终非零退出）：
 *  - Rust：`cargo test --workspace --exclude aether-tauri` + `cargo test -p aether-tauri`
 *    + clippy + deny；
 *  - 前端/协议：`pnpm -r test/typecheck/build`、protocol check、error-codes、
 *    M9-01 baseline、M9-12 门禁/E2E；
 *  - P0：T4/T5a/T5b/T6/T10/T11/T12/T13（映射见 `p0-p1-mapping.json`）；
 *  - P1：三运行时一致性 / 加密备份恢复 / 规则误配率；
 *  - M4-05/ADR-001：`pnpm smoke:installer --variant test`。
 *
 * 输出：`scripts/test/.tmp/m9-13/regression/results.json` + 每个命令的 `.log`。
 *
 * 用法：
 *   node scripts/test/m9-13/regression.mjs [--skip-tauri] [--only T4,T5a]
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { pnpmCommand, repoRoot } from "../lib/exec.mjs";

const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-13", "regression");
const tmpDir = path.join(repoRoot, "dist-out", "tmp");
mkdirSync(outDir, { recursive: true });
mkdirSync(tmpDir, { recursive: true });
const outPath = path.join(outDir, "results.json");
const bundleDir = path.join(repoRoot, "crates", "aether-tauri", "runtime-bundle");
const exe = process.platform === "win32" ? ".exe" : "";
const adapterEnv = {
  AETHER_CLAUDE_ADAPTER: path.join(bundleDir, `aether-claude-adapter${exe}`),
  AETHER_REQUIRE_CLAUDE_ADAPTER: "1",
  AETHER_CODEX_ADAPTER: path.join(bundleDir, `aether-codex-adapter${exe}`),
  AETHER_REQUIRE_CODEX_ADAPTER: "1",
  AETHER_DSH_ADAPTER: path.join(bundleDir, `aether-dsh-adapter${exe}`),
  AETHER_REQUIRE_DSH_ADAPTER: "1",
  AETHER_MOCK_ADAPTER: path.join(bundleDir, `aether-mock-adapter${exe}`),
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_REQUIRE_REAL_CLAUDE: "1",
};
const pnpm = pnpmCommand();
const only = (() => {
  const index = process.argv.indexOf("--only");
  return index >= 0 ? new Set(process.argv[index + 1].split(",")) : null;
})();
const skipTauri = process.argv.includes("--skip-tauri");

const commands = [
  {
    id: "workspace-rust",
    name: "cargo test --workspace --exclude aether-tauri",
    command: [
      "cargo",
      ["test", "--workspace", "--exclude", "aether-tauri", "-j", "1", "--", "--test-threads=1"],
    ],
    timeout: 45 * 60 * 1000,
  },
  ...(skipTauri
    ? []
    : [
        {
          id: "workspace-tauri",
          name: "cargo test -p aether-tauri（G8 前全量；Tauri 仅开发对照）",
          command: [
            "cargo",
            ["test", "-p", "aether-tauri", "-j", "1", "--", "--test-threads=1"],
          ],
          timeout: 60 * 60 * 1000,
        },
      ]),
  {
    id: "frontend-test",
    name: "pnpm -r test",
    command: [pnpm.command, [...pnpm.prefix, "-r", "test"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "frontend-typecheck",
    name: "pnpm -r typecheck",
    command: [pnpm.command, [...pnpm.prefix, "-r", "typecheck"]],
    timeout: 15 * 60 * 1000,
  },
  {
    id: "frontend-build",
    name: "pnpm -r build",
    command: [pnpm.command, [...pnpm.prefix, "-r", "build"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "clippy",
    name: "cargo clippy --workspace --all-targets -- -D warnings",
    command: ["cargo", ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]],
    timeout: 45 * 60 * 1000,
  },
  {
    id: "deny",
    name: "cargo deny check",
    command: ["cargo", ["deny", "check"]],
    timeout: 15 * 60 * 1000,
  },
  {
    id: "baseline",
    name: "pnpm verify:m9-01-baseline",
    command: [pnpm.command, [...pnpm.prefix, "verify:m9-01-baseline"]],
    timeout: 5 * 60 * 1000,
  },
  {
    id: "protocol",
    name: "pnpm --filter @aether/protocol check",
    command: [pnpm.command, [...pnpm.prefix, "--filter", "@aether/protocol", "check"]],
    timeout: 10 * 60 * 1000,
  },
  {
    id: "error-codes",
    name: "pnpm verify:error-codes",
    command: [pnpm.command, [...pnpm.prefix, "verify:error-codes"]],
    timeout: 5 * 60 * 1000,
  },
  {
    id: "security",
    name: "pnpm verify:security（M1-08 → M9-10）",
    command: [pnpm.command, [...pnpm.prefix, "verify:security"]],
    timeout: 15 * 60 * 1000,
  },
  {
    id: "m9-12-prepare-e2e",
    name: "pnpm prepare:m9-12:e2e（Mock bundle + 同源 aetherd）",
    command: [pnpm.command, [...pnpm.prefix, "prepare:m9-12:e2e"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "m9-12-e2e",
    name: "pnpm e2e:m9-12（13 条用户路径复跑）",
    command: [pnpm.command, [...pnpm.prefix, "e2e:m9-12"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "m9-12-verify",
    name: "pnpm verify:m9-12",
    command: [pnpm.command, [...pnpm.prefix, "verify:m9-12"]],
    timeout: 10 * 60 * 1000,
  },
  {
    id: "smoke-installer",
    name: "pnpm smoke:installer --variant test（M4-05/ADR-001 → M9-11）",
    command: [pnpm.command, [...pnpm.prefix, "smoke:installer", "--variant", "test"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "T4",
    name: "T4 kill -9 ×20（M3-06，共享 aether-app 核心）",
    command: [
      "cargo",
      [
        "test",
        "-p",
        "aether-tauri",
        "--test",
        "m3_06_t4",
        "-j",
        "1",
        "--",
        "--test-threads=1",
        "--nocapture",
      ],
    ],
    timeout: 15 * 60 * 1000,
  },
  {
    id: "T5a",
    name: "T5a 适配器崩溃自愈（aether-adapters）",
    command: [
      "cargo",
      [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m2_02_t5a",
        "-j",
        "1",
        "--",
        "--test-threads=1",
      ],
    ],
    env: adapterEnv,
    timeout: 15 * 60 * 1000,
  },
  {
    id: "T5b",
    name: "T5b 适配器卡死恢复（aether-adapters）",
    command: [
      "cargo",
      [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m2_08_t5b",
        "-j",
        "1",
        "--",
        "--test-threads=1",
      ],
    ],
    timeout: 15 * 60 * 1000,
  },
  {
    id: "T6",
    name: "T6 100 并发 ask（aether-control）",
    command: [
      "cargo",
      [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_03_permission",
        "t6_100_concurrent_asks_no_loss_no_duplication",
        "-j",
        "1",
        "--",
        "--test-threads=1",
      ],
    ],
    timeout: 10 * 60 * 1000,
  },
  {
    id: "T10",
    name: "T10 协议健壮性（aether-rpc）",
    command: ["cargo", ["test", "-p", "aether-rpc", "-j", "1", "--", "--test-threads=1"]],
    timeout: 10 * 60 * 1000,
  },
  {
    id: "T12",
    name: "T12 双开防护（M9-07 单实例场景；先于 T11 避免 cargo test 重建 aetherd）",
    command: [process.execPath, [path.join(repoRoot, "scripts", "test", "m9-13", "t12-single-instance.mjs")]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "T11",
    name: "T11 退出可靠性（M9-09 退出场景；T1 时序由 performance 覆盖）",
    command: [process.execPath, [path.join(repoRoot, "scripts", "test", "m9-13", "t11-exit.mjs")]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "T13",
    name: "T13 A4 同步盘防护（M9-09 门禁）",
    command: [pnpm.command, [...pnpm.prefix, "verify:m9-09"]],
    timeout: 10 * 60 * 1000,
  },
  {
    id: "P1-three-runtimes",
    name: "P1 三运行时一致性（M2-02/M2-11）",
    command: [
      "cargo",
      [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m2_02_consistency",
        "--test",
        "m2_11_codex",
        "--test",
        "m2_11_dsh",
        "-j",
        "1",
        "--",
        "--test-threads=1",
      ],
    ],
    env: adapterEnv,
    timeout: 30 * 60 * 1000,
  },
  {
    id: "P1-encrypted-backup",
    name: "P1 加密备份恢复演练（M5-03）",
    command: [pnpm.command, [...pnpm.prefix, "verify:m5-03:drill"]],
    timeout: 20 * 60 * 1000,
  },
  {
    id: "P1-rule-misconfiguration",
    name: "P1 规则误配率（M6-02）",
    command: [pnpm.command, [...pnpm.prefix, "verify:m6-02"]],
    timeout: 10 * 60 * 1000,
  },
];

const merge = process.argv.includes("--merge");
let previousResults = [];
if (merge && existsSync(outPath)) {
  try {
    previousResults = JSON.parse(readFileSync(outPath, "utf8")).commands ?? [];
  } catch {
    previousResults = [];
  }
}
const results = [];
for (const entry of commands) {
  if (only && !only.has(entry.id)) continue;
  const [name, args] = entry.command;
  const logPath = path.join(outDir, `${entry.id}.log`);
  console.log(`\n===== ${entry.id}：${entry.name} =====`);
  console.log(`$ ${[name, ...args].join(" ")}`);
  const started = Date.now();
  const result = spawnSync(name, args, {
    cwd: entry.cwd ?? repoRoot,
    env: {
      ...process.env,
      ...(entry.env ?? {}),
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
      CARGO_PROFILE_TEST_DEBUG: process.env.CARGO_PROFILE_TEST_DEBUG ?? "0",
      CARGO_PROFILE_TEST_CODEGEN_UNITS: process.env.CARGO_PROFILE_TEST_CODEGEN_UNITS ?? "1",
      CARGO_INCREMENTAL: process.env.CARGO_INCREMENTAL ?? "0",
      TEMP: tmpDir,
      TMP: tmpDir,
    },
    encoding: "utf8",
    maxBuffer: 128 * 1024 * 1024,
    timeout: entry.timeout ?? 15 * 60 * 1000,
  });
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  writeFileSync(logPath, output, "utf8");
  const tail = output.trim().split(/\r?\n/).slice(-25).join("\n");
  console.log(tail);
  const exit = result.status ?? 1;
  const timedOut = result.error?.code === "ETIMEDOUT";
  const ok = exit === 0 && !timedOut;
  if (!ok) {
    console.log(
      `::error title=M9-13 regression ${entry.id}::${tail.replace(/\r?\n/g, " | ").slice(-900)}`,
    );
  }
  results.push({
    id: entry.id,
    name: entry.name,
    command: [name, ...args].join(" "),
    exit,
    ok,
    timed_out: timedOut,
    duration_ms: Date.now() - started,
    log: path.relative(repoRoot, logPath).split(path.sep).join("/"),
    tail: tail.slice(-4000),
  });
}

const merged = merge
  ? [
      ...previousResults.filter((item) => !results.some((next) => next.id === item.id)),
      ...results,
    ].sort((left, right) => {
      const order = commands.map((entry) => entry.id);
      return order.indexOf(left.id) - order.indexOf(right.id);
    })
  : results;
const passed = merged.every((item) => item.ok);
const evidence = {
  schema: "aether.m9-13.regression.v1",
  task: "M9-13",
  finished_at: new Date().toISOString(),
  platform: { os: process.platform, arch: process.arch, node: process.version },
  commands: merged,
  passed,
};
writeFileSync(outPath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
console.log(`\n[m9-13-regression] 证据：${path.relative(repoRoot, outPath)}`);
if (!passed) {
  console.error(`[m9-13-regression] FAIL ${results.filter((item) => !item.ok).length} 项`);
  process.exit(1);
}
console.log("[m9-13-regression] PASS");
console.log("AETHER_M9_13_REGRESSION PASS");
