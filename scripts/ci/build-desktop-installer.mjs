#!/usr/bin/env node
/**
 * 构建平台安装器（Windows MSI / macOS DMG）并复制到 dist-out/。
 *
 * 安装器唯一产出方（ADR-001：cargo-dist 不生成安装器，仅编排上传）。
 * 由 release workflow 的 desktop-installers 作业调用：
 *   - Windows runner  -> MSI（WiX，WebView2 embedBootstrapper 内嵌）
 *   - macOS runner    -> DMG
 *   - Linux           -> 打包顺延 P1（设计 §2.1 平台交付），直接跳过
 *
 * 用法：node scripts/ci/build-desktop-installer.mjs [--target <triple>]
 */
import { cpSync, existsSync, mkdirSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { pnpmCommand, repoRoot, run } from "../test/lib/exec.mjs";

function parseTarget() {
  const index = process.argv.indexOf("--target");
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : "";
}

const target = parseTarget() || process.env.AETHER_TARGET || "";
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
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

const bundleKind = process.platform === "win32" ? "msi" : process.platform === "darwin" ? "dmg" : null;
if (bundleKind === null) {
  console.log("[installer] Linux 打包顺延 P1（设计 §2.1），跳过");
  process.exit(0);
}
if (!existsSync(tauriCli)) {
  const install = run(pnpm, [...pnpmPrefix, "install", "--frozen-lockfile"]);
  if (install !== 0) process.exit(install);
}

let code = run(pnpm, [...pnpmPrefix, "-r", "--if-present", "build"]);
if (code !== 0) process.exit(code);

const args = [tauriCli, "build", "--bundles", bundleKind];
if (target) args.push("--target", target);
code = run(process.execPath, args, { cwd: tauriDir });
if (code !== 0) process.exit(code);

const bundleRoot = path.join(
  repoRoot,
  "target",
  ...(target ? [target] : []),
  "release",
  "bundle",
  bundleKind,
);
if (!existsSync(bundleRoot)) {
  console.error(`[installer] 未找到安装器输出目录：${bundleRoot}`);
  process.exit(1);
}
const files = readdirSync(bundleRoot).filter((name) => name.endsWith(`.${bundleKind}`));
if (files.length === 0) {
  console.error(`[installer] 目录中未找到 .${bundleKind} 文件：${bundleRoot}`);
  process.exit(1);
}

const outDir = path.join(repoRoot, "dist-out");
mkdirSync(outDir, { recursive: true });
for (const file of files) {
  const source = path.join(bundleRoot, file);
  cpSync(source, path.join(outDir, file));
  console.log(`[installer] 产出 ${path.join("dist-out", file)}`);
}
