#!/usr/bin/env node
/**
 * T14 类型契约门禁（M3-01；设计 D7 / AGENTS §2.8）：
 * 重新生成 `packages/protocol/src/bindings.ts`，再以 `git diff --exit-code` 校验
 * 生成物与仓库版本一致（禁止手改；CI/本地共用）。
 *
 * 用法：
 *   node scripts/ci/bindings.mjs generate   # 仅生成（写入工作区）
 *   node scripts/ci/bindings.mjs check      # 生成 + 校验（默认）
 *
 * 校验口径：
 * - 生成物必须入库（`git ls-files` 命中；未跟踪即阻断）；
 * - `git diff --exit-code`（工作区 vs 索引）必须为空——捕捉手改/重新生成漂移；
 * - 生成物已在 HEAD 中时追加 `git diff --cached --exit-code`（索引 vs HEAD）；
 *   本地提交前（新增文件已暂存）跳过该步并明示（CI 在提交后执行完整校验）。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const target = "packages/protocol/src/bindings.ts";
const mode = process.argv[2] ?? "check";

if (mode !== "generate" && mode !== "check") {
  console.error(`[bindings] 未知模式：${mode}（可选 generate / check）`);
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

/** 生成（tests/export_bindings.rs 为显式忽略测试，`--ignored` 运行）。 */
function generate() {
  const result = spawnSync(
    tool("cargo"),
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "export_bindings",
      "--",
      "--ignored",
      "--nocapture",
    ],
    { cwd: root, stdio: "inherit", env: process.env },
  );
  if (result.error) {
    console.error(`[bindings] 无法执行 cargo：${result.error.message}`);
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

const generated = generate();
if (generated !== 0) {
  console.error(`[bindings] 生成失败（exit=${generated}）`);
  process.exit(generated);
}

if (mode === "generate") {
  console.log(`[bindings] 生成完成：${target}`);
  process.exit(0);
}

const tracked = git(["ls-files", "--error-unmatch", target], { capture: true });
if (tracked.status !== 0) {
  console.error(
    `[bindings] 阻断：生成物未入库（${target} 未跟踪）。生成物必须随提交入库（AGENTS §2.8）。`,
  );
  process.exit(1);
}

const diff = git(["diff", "--exit-code", "--", target]);
if (diff.status !== 0) {
  console.error("[bindings] 阻断：生成物与重新生成结果不一致（T14，git diff --exit-code）。");
  process.exit(diff.status ?? 1);
}

const inHead = git(["cat-file", "-e", `HEAD:${target}`], { capture: true });
if (inHead.status === 0) {
  const staged = git(["diff", "--cached", "--exit-code", "--", target]);
  if (staged.status !== 0) {
    console.error("[bindings] 阻断：生成物与 HEAD 版本不一致（T14，索引 vs HEAD）。");
    process.exit(staged.status ?? 1);
  }
} else {
  console.log("[bindings] 提示：生成物为本地新增（尚未入 HEAD）；提交后由 CI 执行完整 T14 校验。");
}

console.log(`[bindings] 通过：${target} 与重新生成结果一致（T14）`);
console.log("AETHER_BINDINGS_CHECK PASS");
