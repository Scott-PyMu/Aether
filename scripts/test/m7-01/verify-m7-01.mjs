#!/usr/bin/env node
/**
 * M7-01 验证脚本：官方适配器热插拔与注册清单（ADR-015；DoD0–DoD7）。
 *
 * 覆盖：
 * - 静态：迁移 0006 形态、命令面注册与 bindings 生成物、`runtimes.enabled` 仅经
 *   启停命令维护（无 settings 旁路）、UI 锚点/文案、注册清单信任锚落点；
 * - DoD0/1/2/4：`m7_01_hotplug`（事件链路/状态机/在途收口/10 次热插拔循环；
 *   真实 `aether-adapter-fixture` 进程）；
 * - DoD3：`m7_01_registry`（DI 用例 + 编译期期望表 ↔ 真实运行时包同源用例）；
 * - DoD5：`m7_01_runtime_control` + `ipc_validation`（畸形参数矩阵）+ 前端
 *   `m7_01_runtime_toggle.test.tsx`；
 * - DoD6：`verify:m2-08`（T5a/T5b 回归，可 `--skip-m2-08`）；
 * - DoD7：`verify:m1-03`（迁移 ↔ 附录 C ↔ 真实库双向比对）。
 *
 * 用法：node scripts/test/m7-01/verify-m7-01.mjs [--skip-bundle] [--skip-m2-08]
 *   --skip-bundle：跳过 bun 运行时包构建与「编译期期望同源」忽略用例（显式 SKIP）。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const skipBundle = process.argv.includes("--skip-bundle");
const skipM208 = process.argv.includes("--skip-m2-08");
const skipE2e = process.argv.includes("--skip-e2e");

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const fixture = path.join(repoRoot, "target", "debug", `aether-adapter-fixture${exeSuffix}`);

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-01", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordSkip = (name, reason) =>
  checks.push({ name, exit: 0, expect: 0, skip: true, reason });

// ===== 静态检查（一次 try/catch → 单条记录） =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // 1) 迁移 0006：runtimes.enabled 增量形态。
  const migration = read("migrations/0006_runtime_enabled.sql");
  assert(
    migration.includes("ALTER TABLE runtimes ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1"),
    "0006 必须为 runtimes.enabled 的 NOT NULL DEFAULT 1 增量",
  );
  for (const published of [
    "0001_init.sql",
    "0002_unique_keys.sql",
    "0003_p0_ui_extensions.sql",
    "0004_permission_mode_and_rules.sql",
    "0005_audit_indexes.sql",
  ]) {
    assert(existsSync(path.join(repoRoot, "migrations", published)), `迁移缺失：${published}`);
  }

  // 2) 命令面注册（collected + debug/release handler 三处）与生成物。
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const runtimeDisableMentions = (commands.match(/runtime_disable/g) ?? []).length;
  assert(runtimeDisableMentions >= 4, `runtime_disable 注册点不足：${runtimeDisableMentions}`);
  const bindings = read("packages/protocol/src/bindings.ts");
  assert(bindings.includes("runtimeDisable"), "bindings.ts 必须含 runtimeDisable（T14）");
  assert(bindings.includes('"runtime_busy"'), "bindings.ts 必须含 runtime_busy 错误码");

  // 3) `runtimes.enabled` 仅经启停命令维护（无 settings 等旁路）。
  const allowedSetRuntimeEnabled = new Set([
    "crates/aether-store/src/ops.rs",
    "crates/aether-app/src/runtime_control.rs",
    "crates/aether-store/tests/m7_01_runtime_enabled.rs",
    "crates/aether-tauri/tests/m7_01_runtime_control.rs",
  ]);
  const offender = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name === "node_modules" || entry.name === "target") continue;
        walk(full);
      } else if (entry.name.endsWith(".rs")) {
        const text = readFileSync(full, "utf8");
        if (text.includes("SetRuntimeEnabled")) {
          const relative = path.relative(repoRoot, full).split(path.sep).join("/");
          if (!allowedSetRuntimeEnabled.has(relative)) offender.push(relative);
        }
      }
    }
  };
  walk(path.join(repoRoot, "crates"));
  assert(offender.length === 0, `enabled 写路径出现未登记调用方：${offender.join(",")}`);
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  assert(!/SETTINGS_KEY_ALLOWLIST[\s\S]*?"runtimes\./.test(dto), "settings 不得登记 runtimes.* 键");

  // 4) UI 锚点与文案（ADR-015 附录 E）。
  const panel = read("apps/desktop/src/RuntimePanel.tsx");
  for (const anchor of ["runtime-toggle", "runtime-disable-confirm", "data-count"]) {
    assert(panel.includes(anchor), `RuntimePanel 缺锚点：${anchor}`);
  }
  const sessionTs = read("apps/desktop/src/session.ts");
  assert(sessionTs.includes("user_disabled"), "会话文案必须含 user_disabled");
  const permissionTs = read("apps/desktop/src/permission.ts");
  assert(permissionTs.includes("disableRuntime"), "permission.ts 必须提供 disableRuntime");

  // 5) 注册清单信任锚落点（ADR-015 §2.2#2）。
  const buildRs = read("crates/aether-tauri/build.rs");
  assert(buildRs.includes("AETHER_RUNTIME_BUNDLE_DIR"), "build.rs 必须支持期望摘要数据源覆盖");
  assert(buildRs.includes("registry_expectation.rs"), "build.rs 必须生成期望摘要模块");
  const identity = read("crates/aether-app/src/registry_identity.rs");
  assert(identity.includes("include!"), "registry_identity 必须可被 build.rs include");
  const registry = read("crates/aether-app/src/runtime_registry.rs");
  assert(registry.includes("ExpectedRegistry::compiled"), "运行期必须使用编译进二进制的期望摘要");

  record("静态：迁移/命令面/bindings/旁路/UI 锚点/信任锚逐项", 0);
} catch (error) {
  record(`静态检查失败：${error && error.message ? error.message : error}`, 1);
}

// ===== DoD7：存储层（迁移/偏好写读/在途计数） =====
record(
  "DoD7 aether-store m7_01_runtime_enabled（3 用例）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m7_01_runtime_enabled"]),
);

// ===== DoD2/5：命令层（偏好写序/确认门/审计/错误映射，4 用例） =====
record(
  "DoD2/5 aether-tauri m7_01_runtime_control（4 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_01_runtime_control"]),
);

// ===== DoD3：注册清单 DI 用例（3 用例） =====
record(
  "DoD3 aether-tauri m7_01_registry（DI 3 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_01_registry"]),
);

// ===== DoD1：状态机单测回归 =====
record(
  "DoD1 aether-adapters supervisor 单测",
  run(cargo, ["test", "-p", "aether-adapters", "--lib", "supervisor"]),
);

// ===== DoD0/1/2/4：热插拔集成（真实夹具进程） =====
record(
  "构建 aether-adapter-fixture",
  run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]),
);
if (!existsSync(fixture)) {
  record(`夹具产物缺失：${path.relative(repoRoot, fixture)}`, 1);
} else {
  const fixtureEnv = {
    ...process.env,
    AETHER_FIXTURE_BIN: fixture,
    AETHER_REQUIRE_FIXTURE: "1",
    AETHER_M7_01_EVIDENCE_DIR: evidenceDir,
  };
  record(
    "DoD0/1/2/4 aether-tauri m7_01_hotplug（3 用例；事件链路/在途收口/10 次循环）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_01_hotplug", "--", "--nocapture"], {
      env: fixtureEnv,
    }),
  );
}

// ===== DoD5：IPC 畸形参数矩阵与既有运行时控制回归 =====
record(
  "DoD5 aether-tauri ipc_validation（含 runtime_disable 样本）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);
record(
  "M2-01 runtime_retry/enable 回归（aether-tauri runtime_control）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "runtime_control"]),
);

// ===== DoD5（UI）：启停开关与二次确认（vitest） =====
record(
  "DoD5 前端 m7_01_runtime_toggle（8 用例）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m7_01_runtime_toggle.test.tsx",
  ]),
);

// ===== DoD6：T5a/T5b 回归（热插拔后崩溃恢复口径） =====
if (skipM208) {
  recordSkip("DoD6 verify:m2-08（T5a/T5b）", "--skip-m2-08 显式跳过");
} else {
  record(
    "DoD6 verify:m2-08（T5a/T5b + 孤儿清理 + T11）",
    run(process.execPath, ["scripts/test/m2-08/verify-m2-08.mjs"], { cwd: repoRoot }),
  );
}

// ===== DoD7：迁移 ↔ 附录 C ↔ 真实库双向比对 =====
record(
  "DoD7 verify:m1-03（含 0006 ↔ 附录 C）",
  run(process.execPath, ["scripts/test/m1-03/verify-m1-03.mjs"], { cwd: repoRoot }),
);

// ===== DoD3：编译期期望 ↔ 真实运行时包（M4-05 产物接口一致性） =====
if (skipBundle) {
  recordSkip("DoD3 编译期期望 ↔ 真实运行时包", "--skip-bundle 显式跳过");
} else {
  const bun = resolveBun();
  const bunExit = run(bun, ["--version"], { capture: true });
  if (bunExit !== 0) {
    recordSkip(
      "DoD3 编译期期望 ↔ 真实运行时包",
      `bun 不可用（${bun}，exit=${bunExit}）——显式跳过，禁止静默通过`,
    );
  } else {
    const bundleDir = path.join(evidenceDir, "runtime-bundle");
    const bundleExit = run(
      process.execPath,
      [path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs"), "--out", bundleDir],
      { env: { ...process.env, AETHER_BUNDLE_MOCK: "1" } },
    );
    record("DoD3 构建测试运行时包（含 mock）", bundleExit);
    if (bundleExit === 0) {
      record(
        "DoD3 编译期期望 ↔ 真实包（ignored 用例；AETHER_RUNTIME_BUNDLE_DIR 同源）",
        run(
          cargo,
          [
            "test",
            "-p",
            "aether-tauri",
            "--test",
            "m7_01_registry",
            "--",
            "--ignored",
            "--nocapture",
          ],
          {
            env: {
              ...process.env,
              AETHER_RUNTIME_BUNDLE_DIR: bundleDir,
              AETHER_M7_01_BUNDLE_DIR: bundleDir,
            },
          },
        ),
      );
    }
  }
}

// ===== 证据归档 =====
const summary = {
  task: "M7-01",
  stamp,
  platform: process.platform,
  fixture: path.relative(repoRoot, fixture),
  evidence_dir: path.relative(repoRoot, evidenceDir),
  skip: { bundle: skipBundle, m2_08: skipM208, e2e: skipE2e },
  checks,
};
writeFileSync(path.join(evidenceDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`, "utf8");
console.log(`[m7-01] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-01", checks));
