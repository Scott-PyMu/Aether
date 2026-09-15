/**
 * 桌面壳冒烟测试（M1-01 DoD1：Win/mac 构建+冒烟）。
 *
 * 校验构建产物（优先 debug）：
 *   1) `aether-tauri --version` 输出与单一来源版本一致；
 *   2) `aether-tauri --aether-diagnostics` 输出包含核心层版本。
 *
 * 用法：node scripts/test/smoke-desktop.mjs [--binary <path>]
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { repoRoot, summarize } from "./lib/exec.mjs";

function parseArgs() {
  const args = process.argv.slice(2);
  const binaryIndex = args.indexOf("--binary");
  if (binaryIndex >= 0 && args[binaryIndex + 1]) return path.resolve(args[binaryIndex + 1]);
  const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
  const candidates = [
    process.env.AETHER_BINARY,
    path.join(repoRoot, "target", "debug", binaryName),
    path.join(repoRoot, "target", "release", binaryName),
  ].filter((candidate) => typeof candidate === "string" && candidate.length > 0);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

function sourceVersion() {
  const result = spawnSync(
    process.execPath,
    [path.join(repoRoot, "scripts", "ci", "version.mjs"), "print"],
    { encoding: "utf8" },
  );
  if (result.status !== 0) throw new Error("无法读取单一来源版本");
  return result.stdout.trim();
}

const binary = parseArgs();
if (!binary || !existsSync(binary)) {
  console.error("[smoke] 未找到构建产物；请先执行 `cargo build -p aether-tauri`，或用 --binary 指定路径");
  process.exit(1);
}
console.log(`[smoke] 目标产物：${binary}`);

const version = sourceVersion();
const checks = [];

function capture(args) {
  const result = spawnSync(binary, args, { encoding: "utf8" });
  const stdout = (result.stdout ?? "").trim();
  console.log(`$ ${binary} ${args.join(" ")}\n${stdout}`);
  return { exit: result.status ?? 1, stdout };
}

{
  const { exit, stdout } = capture(["--version"]);
  checks.push({
    name: `--version 输出 "Aether ${version}"`,
    expect: 0,
    exit: exit === 0 && stdout === `Aether ${version}` ? 0 : 1,
  });
}

{
  const { exit, stdout } = capture(["--aether-diagnostics"]);
  const ok = exit === 0 && stdout.includes(`core ${version}`);
  checks.push({
    name: `--aether-diagnostics 输出包含 "core ${version}"`,
    expect: 0,
    exit: ok ? 0 : 1,
  });
}

process.exit(summarize("smoke-desktop", checks));
