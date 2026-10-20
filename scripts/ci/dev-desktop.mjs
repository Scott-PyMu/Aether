#!/usr/bin/env node
/**
 * 调试模式启动器（README「调试模式（开发者模式）」）：`pnpm dev:desktop [-- <tauri dev 参数>]`。
 *
 * 为什么需要它：Tauri CLI 只把**当前工作目录**作为查找 `tauri.conf.json` 的起点，
 * `-c/--config` 也不改变应用目录的识别结果（tauri-cli 2.11 实测：在 `apps/desktop`
 * 下运行 `tauri dev` 报 "Couldn't recognize the current folder as a Tauri project"，
 * 在仓库根加 `--config crates/aether-tauri/tauri.conf.json` 仍是同一错误）。
 * 本仓库的 `tauri.conf.json` 位于 `crates/aether-tauri`（Rust 壳层），
 * `build.beforeDevCommand` 的相对路径（`--dir ../..`）同样按该目录设计——
 * 与 scripts/ci/build-desktop-installer.mjs 的 `tauri build` 同源。
 *
 * 脚本只做三件事：
 *   1) 解析 tauri CLI（apps/desktop 的依赖，不依赖 PATH 里的全局 tauri）；
 *   2) 保证子进程能解析 `cargo` 与 `pnpm`（~/.cargo/bin 通常不在 Windows 用户 PATH，
 *      README 原先要求手工 `$env:Path` 前置，这里自动补齐）；
 *   3) 以 `crates/aether-tauri` 为工作目录运行 `tauri dev`，退出码原样透传。
 *
 * 环境变量（AETHER_RUNTIME_BUNDLE_DIR / AETHER_DATA_DIR / AETHER_SECRETS_PASSPHRASE 等）
 * 原样继承；额外参数经 `--` 透传，例如 `pnpm dev:desktop -- --no-watch`。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot } from "../test/lib/exec.mjs";

const tauriDir = path.join(repoRoot, "crates", "aether-tauri");
const tauriConf = path.join(tauriDir, "tauri.conf.json");
const tauriCli = path.join(
  repoRoot,
  "apps",
  "desktop",
  "node_modules",
  "@tauri-apps",
  "cli",
  "tauri.js",
);

if (!existsSync(tauriConf)) {
  console.error(`[dev] 未找到 Tauri 配置：${tauriConf}`);
  process.exit(1);
}
if (!existsSync(tauriCli)) {
  console.error(`[dev] 未找到 Tauri CLI：${tauriCli}`);
  console.error("[dev] 请先在仓库根执行 `pnpm install --frozen-lockfile`");
  process.exit(1);
}

// tauri CLI 会按 PATH 调用 `cargo`；`beforeDevCommand` 经 tauri CLI → pnpm(.cmd) → node。
// 与 build-desktop-installer.mjs 同一策略：解析到绝对路径时把所在目录前置到子进程 PATH。
const extraPath = [];
const cargoBin = bin("cargo");
if (path.isAbsolute(cargoBin)) extraPath.push(path.dirname(cargoBin));
extraPath.push(path.dirname(process.execPath));
for (const candidate of [
  process.env.PNPM_HOME,
  process.env.APPDATA && path.join(process.env.APPDATA, "npm"),
  process.env.LOCALAPPDATA && path.join(process.env.LOCALAPPDATA, "pnpm"),
]) {
  if (candidate && existsSync(candidate)) extraPath.push(candidate);
}

const env = { ...process.env };
const seen = new Set();
const pathPrefix = extraPath.filter((dir) => {
  const key = dir.toLowerCase();
  if (seen.has(key)) return false;
  seen.add(key);
  return true;
});
if (pathPrefix.length > 0) {
  env.PATH = `${pathPrefix.join(path.delimiter)}${path.delimiter}${env.PATH ?? ""}`;
}

const passthrough = process.argv.slice(2);
const args = [tauriCli, "dev", ...passthrough];
console.log(`[dev] 应用目录：${tauriDir}`);
console.log(`[dev] 配置：${tauriConf}`);
console.log(
  `[dev] 命令：tauri dev${passthrough.length > 0 ? ` ${passthrough.join(" ")}` : ""}（Ctrl+C 结束）`,
);

const result = spawnSync(process.execPath, args, {
  cwd: tauriDir,
  stdio: "inherit",
  env,
});
if (result.error) {
  console.error(`[dev] 无法启动 tauri CLI：${result.error.message}`);
  process.exit(127);
}
// Ctrl+C 结束（signal）按 130 退出，避免被误判为普通失败。
process.exit(result.status ?? (result.signal ? 130 : 1));
