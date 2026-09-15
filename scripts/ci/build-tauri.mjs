#!/usr/bin/env node
/**
 * cargo-dist 构建命令：为目标平台构建 Tauri 发布二进制（不产出安装器）。
 *
 * - cargo-dist 通过 CARGO_DIST_TARGET 传入目标 triple；
 * - 前端资源必须先行构建（tauri-build 校验 frontendDist 存在且非空）；
 * - 安装器（MSI/DMG）由 release workflow 的 desktop-installers 作业单独产出。
 */
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { pnpmCommand, repoRoot, run } from "../test/lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const target = process.env.CARGO_DIST_TARGET ?? "";
const tauriDir = path.join(repoRoot, "crates", "aether-tauri");
const tauriCli = path.join(
  repoRoot,
  "apps",
  "desktop",
  "node_modules",
  "@tauri-apps",
  "cli",
  "tauri.js",
);

if (!existsSync(tauriCli)) {
  const install = run(pnpm, [...pnpmPrefix, "install", "--frozen-lockfile"]);
  if (install !== 0) process.exit(install);
}

let code = run(pnpm, [...pnpmPrefix, "-r", "--if-present", "build"]);
if (code !== 0) process.exit(code);

const args = [tauriCli, "build", "--no-bundle"];
if (target) args.push("--target", target);
code = run(process.execPath, args, { cwd: tauriDir });
process.exit(code);
