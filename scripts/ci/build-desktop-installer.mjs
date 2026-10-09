#!/usr/bin/env node
/**
 * 构建平台安装器（Windows MSI / macOS DMG）并复制到输出目录（默认 dist-out/）。
 *
 * 安装器唯一产出方（ADR-001：cargo-dist 不生成安装器，仅编排上传）。
 * 由 release workflow 的 desktop-installers 作业调用：
 *   - Windows runner  -> MSI（WiX，WebView2 embedBootstrapper 内嵌）
 *   - macOS runner    -> DMG
 *   - Linux           -> 打包顺延 P1（设计 §2.1 平台交付），直接跳过
 *
 * 用法：node scripts/ci/build-desktop-installer.mjs
 *   [--target <triple>] [--debug] [--out <dir>]
 *   --debug：调试构建安装器（M4-05 安装产物冒烟用：探针仅 debug 构建可达）
 *   AETHER_BUNDLE_MOCK=1：测试构建，运行时包额外包含 Mock 适配器（Mock 仅测试构建）
 */
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run } from "../test/lib/exec.mjs";

function argValue(flag, fallback) {
  const index = process.argv.indexOf(flag);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

const target = argValue("--target", "") || process.env.AETHER_TARGET || "";
const debug = process.argv.includes("--debug");
const outDir = path.resolve(repoRoot, argValue("--out", "dist-out"));
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

// M4-05：安装产物内置官方运行时注册（三官方集合 + 注册清单；Mock 仅测试构建）。
// 测试构建（打包冒烟闭环）以 AETHER_BUNDLE_MOCK=1 追加 Mock 适配器。
// 注意：debug 冒烟构建若上一次遗留了测试包目录，重建前先由 build-runtime-bundles 清理。
code = run(process.execPath, [path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs")]);
if (code !== 0) process.exit(code);

const args = [tauriCli, "build", "--bundles", bundleKind];
if (debug) args.push("--debug");
if (target) args.push("--target", target);
// tauri CLI 内部按 PATH 调用 `cargo` 与 `beforeBuildCommand` 的 `pnpm`；本地环境
// 经 `bin()`/`pnpmCommand()` 解析（AETHER_CARGO / ~/.cargo/bin / PATH）。解析到
// 绝对路径时把相应目录前置到子进程 PATH，保证 tauri 子壳可解析。
const cargoBin = bin("cargo");
const tauriEnv = { ...process.env };
const extraPath = [];
if (path.isAbsolute(cargoBin)) extraPath.push(path.dirname(cargoBin));
// `beforeBuildCommand` 经 tauri CLI → pnpm(.cmd) → node；确保当前 Node 目录可解析。
extraPath.push(path.dirname(process.execPath));
const npmAppData = process.env.APPDATA ? path.join(process.env.APPDATA, "npm") : "";
const localPnpm = process.env.LOCALAPPDATA ? path.join(process.env.LOCALAPPDATA, "pnpm") : "";
for (const candidate of [process.env.PNPM_HOME, npmAppData, localPnpm]) {
  if (candidate && existsSync(candidate)) extraPath.push(candidate);
}
if (extraPath.length > 0) {
  tauriEnv.PATH = `${extraPath.join(path.delimiter)}${path.delimiter}${tauriEnv.PATH ?? ""}`;
}
code = run(process.execPath, args, { cwd: tauriDir, env: tauriEnv });
if (code !== 0) process.exit(code);

const bundleRoot = path.join(
  repoRoot,
  "target",
  ...(target ? [target] : []),
  debug ? "debug" : "release",
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

mkdirSync(outDir, { recursive: true });
const hashes = [];
for (const file of files) {
  const source = path.join(bundleRoot, file);
  cpSync(source, path.join(outDir, file));
  // M4-05 DoD3：产物哈希记录（sha256）。
  const hash = createHash("sha256").update(readFileSync(source)).digest("hex");
  hashes.push(`${hash}  ${file}`);
  console.log(`[installer] 产出 ${path.relative(repoRoot, path.join(outDir, file))}`);
  console.log(`[installer] sha256 ${file}: ${hash}`);
}
writeFileSync(path.join(outDir, "SHA256SUMS.txt"), `${hashes.join("\n")}\n`, "utf8");
console.log(`[installer] 哈希清单 ${path.relative(repoRoot, path.join(outDir, "SHA256SUMS.txt"))}`);
