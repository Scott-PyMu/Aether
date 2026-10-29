#!/usr/bin/env node
/**
 * `packages/protocol/src/types.ts` 生成器（M9-08 DoD-1/DoD-2；ADR-029 §7）。
 *
 * 生成入口：`crates/aether-app/tests/m9_08_protocol.rs` 的
 * `export_protocol_types`（`--ignored`）。类型来自 `aether-app` 的
 * `specta::Type`（请求 DTO / `IpcError` / `SessionSummary` / `AetherEvent` /
 * `AetherErrorData`），不使用 Tauri 生成器。
 *
 * 生成物禁止手改（AGENTS §2.8）。用法：
 *   node scripts/ci/protocol-types.mjs generate
 *   node scripts/ci/protocol-types.mjs check
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const target = "packages/protocol/src/types.ts";
const mode = process.argv[2] ?? "check";

if (mode !== "generate" && mode !== "check") {
  console.error(`[protocol-types] 未知模式：${mode}（可选 generate / check）`);
  process.exit(2);
}

function tool(name) {
  const override = process.env[`AETHER_${name.toUpperCase()}`];
  if (override) return override;
  const exe = process.platform === "win32" ? ".exe" : "";
  const cargoCandidate = path.join(os.homedir(), ".cargo", "bin", `${name}${exe}`);
  if (existsSync(cargoCandidate)) return cargoCandidate;
  return name;
}

function exportTypes() {
  const result = spawnSync(
    tool("cargo"),
    [
      "test",
      "-p",
      "aether-app",
      "--test",
      "m9_08_protocol",
      "--",
      "--ignored",
      "--nocapture",
    ],
    { cwd: root, stdio: "inherit", env: process.env },
  );
  if (result.error) {
    console.error(`[protocol-types] 无法启动 cargo：${result.error.message}`);
    return 127;
  }
  return result.status ?? 1;
}

function git(args, options = {}) {
  const result = spawnSync("git", args, {
    cwd: root,
    stdio: options.capture ? "pipe" : "inherit",
    encoding: "utf8",
    env: process.env,
  });
  if (result.error) throw new Error(`git ${args.join(" ")} 失败：${result.error.message}`);
  return result;
}

const exported = exportTypes();
if (exported !== 0) {
  console.error(`[protocol-types] 生成失败（exit=${exported}）`);
  process.exit(exported);
}

if (mode === "generate") {
  console.log(`[protocol-types] 生成完成：${target}`);
  process.exit(0);
}

const tracked = git(["ls-files", "--error-unmatch", target], { capture: true });
if (tracked.status !== 0) {
  console.error(
    `[protocol-types] 阻断：生成物未被 git 跟踪（${target}）；生成物必须入库（AGENTS §2.8）。`,
  );
  process.exit(1);
}

const diff = git(["diff", "--exit-code", "--", target]);
if (diff.status !== 0) {
  console.error("[protocol-types] 阻断：生成物与重新生成结果不一致（diff=0 要求）。");
  process.exit(diff.status ?? 1);
}

console.log(`[protocol-types] 通过：${target} 与重新生成结果一致`);
console.log("AETHER_PROTOCOL_TYPES PASS");
