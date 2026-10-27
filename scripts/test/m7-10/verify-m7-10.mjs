#!/usr/bin/env node
/**
 * M7-10 / ADR-028 v0.9 验证（三官方运行时多供应商与按需凭据代理）。
 *
 * 覆盖：
 * - 静态登记：ADR-028 v0.9（21 行真相表、`provider.catalog.get`、静态 `multi_provider`、
 *   补选 `session_busy` 上下文文案、只读/中断、OpenAI `/v1`）、设置键 9、
 *   UI-UX 五个锚点、证据文件存在；
 * - Rust：目录查询协议（coalesce/错误映射/超限不断连）、端点解析/官方默认/OpenAI `/v1`、
 *   21 行真相表逐行、provider 空 model_state（含 model 停用）、broker 集成；
 * - 应用级 E2E：真适配器 + 假 CLI + 真 broker（两会话并发/旧会话补选/状态隔离/
 *   `session_busy`/只读中断/密钥轮换）；缺适配器包时构建（bun）后执行；
 * - TS：SDK 目录客户端/broker env 剥离、Codex/DSH per-session provider、桌面 M7-10 UI；
 * - bindings check + typecheck。
 *
 * 运行：`pnpm verify:m7-10`。
 */
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const node = process.execPath;

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
const assertContains = (text, token, label) => {
  if (!text.includes(token)) throw new Error(`${label} 缺少：${token}`);
};

function staticChecks() {
  const adr = read("docs/adr/ADR-028-provider-catalog-credential-broker.md");
  for (const token of [
    "v0.9",
    "provider.catalog.get",
    "multi_provider",
    "21 行",
    "session_busy",
    "当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。",
    "session_interrupt",
    "/v1",
    "ui.provider_selection",
  ]) {
    assertContains(adr, token, "ADR-028");
  }
  const plan = read("docs/P1-实施计划与验收标准.md");
  for (const token of ["M7-10", "verify:m7-10", "docs/M7-10-证据.md"]) {
    assertContains(plan, token, "P1 计划");
  }
  const uiux = read("docs/UI-UX-设计规格.md");
  for (const token of [
    "session-provider-select",
    "provider-required-notice",
    "provider-endpoint-preview",
    "session-provider-current",
    "session-provider-readonly",
    "data-context=\"create|reselect\"",
  ]) {
    assertContains(uiux, token, "UI-UX");
  }
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  for (const token of [
    "\"ui.provider_selection\"",
    "\"ui.layout\"",
  ]) {
    assertContains(dto, token, "SETTINGS_KEY_ALLOWLIST");
  }
  if (!existsSync(path.join(repoRoot, "docs/M7-10-证据.md"))) {
    throw new Error("缺少 docs/M7-10-证据.md");
  }
  if (!existsSync(path.join(repoRoot, "crates/aether-tauri/tests/m7_10_provider_e2e.rs"))) {
    throw new Error("缺少 m7_10_provider_e2e.rs");
  }
}

function main() {
  try {
    staticChecks();
    record("静态登记（ADR/计划/UI-UX/证据/E2E）", 0);
  } catch (error) {
    console.error(`[m7-10] 静态登记失败：${error.message}`);
    record("静态登记（ADR/计划/UI-UX/证据/E2E）", 1);
  }

  record(
    "目录查询协议（coalesce/错误映射/2MiB 不断连）",
    run(cargo, [
      "test",
      "-p",
      "aether-adapters",
      "--lib",
      "connection::tests::provider_catalog",
    ]),
  );
  record(
    "端点解析/官方默认/OpenAI /v1（provider_env）",
    run(cargo, ["test", "-p", "aether-tauri", "--lib", "provider_env::tests"]),
  );
  record(
    "21 行真相表逐行",
    run(cargo, [
      "test",
      "-p",
      "aether-tauri",
      "--lib",
      "session_backend::tests::selection_truth_table_is_exhaustive",
    ]),
  );
  record(
    "provider 空 model_state（含 model 停用）",
    run(cargo, [
      "test",
      "-p",
      "aether-tauri",
      "--lib",
      "session_backend::tests::provider_selection_state",
    ]),
  );
  record(
    "Credential Broker 集成（token/runtime/protocol）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_10_credential_broker"]),
  );

  // 构建三官方适配器包（bun 单文件），随后执行应用级 E2E。
  const bundle = path.join(repoRoot, "crates", "aether-tauri", "runtime-bundle");
  record(
    "构建三官方适配器包",
    run(node, [path.join(repoRoot, "scripts/ci/build-runtime-bundles.mjs")]),
  );
  const env = {
    ...process.env,
    AETHER_M7_10_REQUIRE: "1",
    AETHER_CLAUDE_ADAPTER: path.join(
      bundle,
      process.platform === "win32" ? "aether-claude-adapter.exe" : "aether-claude-adapter",
    ),
    AETHER_CODEX_ADAPTER: path.join(
      bundle,
      process.platform === "win32" ? "aether-codex-adapter.exe" : "aether-codex-adapter",
    ),
    AETHER_DSH_ADAPTER: path.join(
      bundle,
      process.platform === "win32" ? "aether-dsh-adapter.exe" : "aether-dsh-adapter",
    ),
  };
  record(
    "应用级 E2E（三运行时/并发/旧会话补选/只读/轮换）",
    run(
      cargo,
      ["test", "-p", "aether-tauri", "--test", "m7_10_provider_e2e", "--", "--test-threads=1"],
      { env },
    ),
  );

  record(
    "SDK 目录客户端/broker env 剥离",
    run(pnpm, [
      ...pnpmPrefix,
      "--filter",
      "@aether/adapter-sdk",
      "exec",
      "vitest",
      "run",
      "src/catalog.test.ts",
    ]),
  );
  record(
    "Codex per-session provider",
    run(pnpm, [
      ...pnpmPrefix,
      "--filter",
      "@aether/adapter-codex",
      "exec",
      "vitest",
      "run",
      "src/codex-adapter.test.ts",
      "-t",
      "per-session provider",
    ]),
  );
  record(
    "DSH per-session provider",
    run(pnpm, [
      ...pnpmPrefix,
      "--filter",
      "@aether/adapter-dsh",
      "exec",
      "vitest",
      "run",
      "src/dsh-adapter.test.ts",
      "-t",
      "per-session provider",
    ]),
  );
  record(
    "密钥扫描（非测试源码 0 命中）",
    run(node, [path.join(repoRoot, "scripts/test/m7-10/secret-scan.mjs")]),
  );
  record(
    "桌面 M7-10 UI（冷启动/必选/记忆/补选/只读/端点）",
    run(pnpm, [
      ...pnpmPrefix,
      "--filter",
      "@aether/desktop",
      "exec",
      "vitest",
      "run",
      "src/m7_10_provider_ui.test.tsx",
    ]),
  );
  record(
    "bindings check",
    run(pnpm, [...pnpmPrefix, "--filter", "@aether/protocol", "check"]),
  );
  record(
    "typecheck",
    run(pnpm, [...pnpmPrefix, "-r", "--if-present", "typecheck"]),
  );

  const failed = summarize("M7-10 验证汇总", checks);
  if (failed > 0) {
    console.error("[m7-10] 存在未通过项");
    process.exit(1);
  }
  console.log("[m7-10] 全部通过");
}

main();
