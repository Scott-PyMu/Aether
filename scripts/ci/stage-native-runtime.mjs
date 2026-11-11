#!/usr/bin/env node
/**
 * M10-05/ADR-035：native 解码依赖真实文件 staging。
 *
 * pnpm 的 `apps/desktop/node_modules/@napi-rs/**` 是 junction，electron-builder 的
 * `files` glob 不会收录（M10-05 证据 §5.1）。本脚本把：
 *   - `@napi-rs/image`（JS + package.json）
 *   - 当前平台的 `@napi-rs/image-<platform>-<arch>-<abi>`（含 `.node`）
 *   - `omggif`（JS + package.json）
 * 以**真实文件**复制到 `apps/desktop/native-runtime/node_modules/**`；
 * electron-builder `files`/`asarUnpack` 只打包该目录（只含当前平台，禁止全平台）。
 *
 * 运行：node scripts/ci/stage-native-runtime.mjs
 */
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const appDir = path.join(root, "apps", "desktop");
const target = path.join(appDir, "native-runtime", "node_modules");
const requireFromApp = createRequire(path.join(appDir, "package.json"));
void requireFromApp;

const PLATFORM_PACKAGES = {
  "win32-x64": "image-win32-x64-msvc",
  "win32-arm64": "image-win32-arm64-msvc",
  "darwin-x64": "image-darwin-x64",
  "darwin-arm64": "image-darwin-arm64",
  "linux-x64": "image-linux-x64-gnu",
  "linux-arm64": "image-linux-arm64-gnu",
};

function packageDir(specifier, from = path.join(appDir, "package.json")) {
  return path.dirname(createRequire(from).resolve(`${specifier}/package.json`));
}

function copyPackage(source, destination) {
  if (!existsSync(source)) {
    throw new Error(`native 依赖缺失：${source}`);
  }
  mkdirSync(path.dirname(destination), { recursive: true });
  // dereference: true —— 把 pnpm junction 展开为真实文件。
  cpSync(source, destination, { recursive: true, dereference: true });
}

const platformKey = `${process.platform}-${process.arch}`;
const platformPackage = PLATFORM_PACKAGES[platformKey];
if (!platformPackage) {
  throw new Error(`不支持的原生平台组合：${platformKey}（ADR-035 仅三平台）`);
}

rmSync(path.join(appDir, "native-runtime"), { recursive: true, force: true });
mkdirSync(target, { recursive: true });

const imageDir = packageDir("@napi-rs/image");
const imagePkg = JSON.parse(readFileSync(path.join(imageDir, "package.json"), "utf8"));
const platformDir = packageDir(
  `@napi-rs/${platformPackage}`,
  path.join(imageDir, "package.json"),
);
const platformPkg = JSON.parse(readFileSync(path.join(platformDir, "package.json"), "utf8"));
const omggifDir = packageDir("omggif");
const omggifPkg = JSON.parse(readFileSync(path.join(omggifDir, "package.json"), "utf8"));

copyPackage(imageDir, path.join(target, "@napi-rs", "image"));
copyPackage(platformDir, path.join(target, "@napi-rs", platformPackage));
copyPackage(omggifDir, path.join(target, "omggif"));

// 断言：真实文件（非 junction）、.node 存在、版本/许可证符合 ADR-035。
const nodeFiles = [];
function collect(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isSymbolicLink()) {
      throw new Error(`native-runtime 不允许符号链接：${full}`);
    }
    if (entry.isDirectory()) collect(full);
    else if (entry.name.endsWith(".node")) nodeFiles.push(full);
  }
}
collect(target);
if (nodeFiles.length === 0) {
  throw new Error("native-runtime 缺少 .node 文件（asarUnpack 将无法加载）");
}
if (imagePkg.version !== "1.16.0" || imagePkg.license !== "MIT") {
  throw new Error(`@napi-rs/image 版本/许可证不符：${imagePkg.version} ${imagePkg.license}`);
}
if (platformPkg.version !== imagePkg.version) {
  throw new Error(
    `@napi-rs/${platformPackage} 版本 ${platformPkg.version} ≠ image ${imagePkg.version}`,
  );
}
if (omggifPkg.version !== "1.0.10" || omggifPkg.license !== "MIT") {
  throw new Error(`omggif 版本/许可证不符：${omggifPkg.version} ${omggifPkg.license}`);
}
const bytes = nodeFiles.reduce((sum, file) => sum + statSync(file).size, 0);
console.log(
  `[stage-native-runtime] ${platformKey}: @napi-rs/image ${imagePkg.version} + ${platformPackage} + omggif ${omggifPkg.version}；` +
    `.node ${nodeFiles.length} 个 / ${bytes} B；target=${path.relative(root, target)}`,
);
