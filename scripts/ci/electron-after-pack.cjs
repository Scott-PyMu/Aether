/**
 * electron-builder `afterPack` 钩子（M9-11；ADR-029 v0.4 §5.1/§8.1）。
 *
 * 运行时机：应用目录已生成、**签名之前**。职责：
 *  1. 断言 `extraResources` 的 `bin/aetherd` 与 `runtime-bundle/` 已就位；
 *  2. 断言 asar 内不含 sidecar（`aetherd`/适配器必须走 extraResources）；
 *  3. 翻转 `@electron/fuses` 并断言（复用 M9-10 的 `scripts/ci/electron-fuses.mjs`）；
 *  4. macOS：先签 `aetherd` 与适配器（同一 Team ID），随后由 electron-builder 签
 *     `.app` 与 DMG —— 满足「嵌套 → 应用 → DMG」的签名顺序（DoD-2）。
 *
 * 仅使用 Node 标准库 + 仓库脚本；不引入新依赖。
 */
"use strict";

const { chmodSync, existsSync, readFileSync, readdirSync, rmSync, statSync } = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { pathToFileURL } = require("node:url");

const REPO_ROOT = path.resolve(__dirname, "..", "..");

function fail(message) {
  throw new Error(`[after-pack] ${message}`);
}

function resourcesDir(appOutDir, productFilename, platform) {
  return platform === "darwin"
    ? path.join(appOutDir, `${productFilename}.app`, "Contents", "Resources")
    : path.join(appOutDir, "resources");
}

function appBinary(appOutDir, productFilename, platform) {
  if (platform === "darwin") {
    return path.join(appOutDir, `${productFilename}.app`, "Contents", "MacOS", productFilename);
  }
  if (platform === "win32") {
    return path.join(appOutDir, `${productFilename}.exe`);
  }
  return path.join(appOutDir, productFilename.toLowerCase());
}

function runNode(args) {
  const result = spawnSync(process.execPath, args, {
    cwd: REPO_ROOT,
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) fail(`无法执行 ${process.execPath}：${result.error.message}`);
  if (result.status !== 0) fail(`命令失败（exit=${result.status}）：${args.join(" ")}`);
}

function assertRuntimeBundle(resources, variant) {
  const bundleDir = path.join(resources, "runtime-bundle");
  const manifestPath = path.join(bundleDir, "runtimes.json");
  const digestsPath = path.join(bundleDir, "digests.json");
  if (!existsSync(manifestPath)) fail(`缺少 runtime-bundle/runtimes.json：${manifestPath}`);
  if (!existsSync(digestsPath)) fail(`缺少 runtime-bundle/digests.json：${digestsPath}`);
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const ids = (manifest.runtimes ?? []).map((entry) => entry.id);
  const expected = ["claude-code", "codex", "deepseek-harness"];
  for (const id of expected) {
    if (!ids.includes(id)) fail(`runtime-bundle 缺少官方运行时：${id}`);
  }
  if (variant === "release" && ids.includes("mock")) {
    fail("release 构建的 runtime-bundle 不得包含 mock（ADR-008）");
  }
  if (variant === "test" && !ids.includes("mock")) {
    fail("test 构建的 runtime-bundle 必须包含 mock（安装冒烟用）");
  }
  for (const entry of manifest.runtimes ?? []) {
    const program = path.join(bundleDir, entry.program);
    if (!existsSync(program)) fail(`runtime-bundle 缺少程序：${entry.program}`);
  }
  return ids;
}

function assertAsar(resources) {
  const asarPath = path.join(resources, "app.asar");
  if (!existsSync(asarPath)) fail(`缺少 app.asar：${asarPath}`);
  // 壳级页面必须在 asar 外（file:// 读不到 asar 内文件；见 shellPaths.ts）。
  const unpackedShell = path.join(
    resources,
    "app.asar.unpacked",
    "apps",
    "desktop",
    "electron",
    "dist",
    "shell",
    "startup",
    "startup.html",
  );
  if (!existsSync(unpackedShell)) {
    fail(`壳级页面未解包（asarUnpack）：${unpackedShell}`);
  }
  // 动态 import 仓库内的 ESM 辅助模块（afterPack 为 CJS）。
  return import(pathToFileURL(path.join(REPO_ROOT, "scripts", "ci", "asar.mjs")).href)
    .then((module) => {
      const files = module.listAsarFiles(asarPath);
      for (const forbidden of ["aetherd", "aetherd.exe", "aether-claude-adapter", "aether-codex-adapter", "aether-dsh-adapter", "aether-mock-adapter"]) {
        if (module.asarContainsBasename(files, forbidden)) {
          fail(`asar 内出现 sidecar（应走 extraResources）：${forbidden}`);
        }
      }
      for (const required of [
        "/apps/desktop/dist/index.html",
        "/apps/desktop/electron/dist/main.cjs",
        "/apps/desktop/electron/dist/preload.cjs",
      ]) {
        if (!files.includes(required)) fail(`asar 缺少入口：${required}`);
      }
      return files.length;
    });
}

function pathToFileUrl(file) {
  return pathToFileURL(file).href;
}

function signMacNested(resources) {
  const identity = process.env.AETHER_MAC_IDENTITY || process.env.CSC_NAME;
  if (!identity) {
    // electron-builder 仍可完成 `.app` 签名；但 DoD-2 要求嵌套先签，发布门禁由
    // `pnpm verify:m9-11` 在签名产物上校验（无身份 → 明确失败，不静默通过）。
    console.warn(
      "[after-pack] 未配置 CSC_NAME/AETHER_MAC_IDENTITY：跳过嵌套预签名；" +
        "G6 签名门禁将失败（无证书不得宣称通过）。",
    );
    return [];
  }
  const targets = [];
  const binDir = path.join(resources, "bin");
  if (existsSync(binDir)) {
    for (const name of readdirSync(binDir)) {
      targets.push(path.join(binDir, name));
    }
  }
  const bundleDir = path.join(resources, "runtime-bundle");
  if (existsSync(bundleDir)) {
    for (const name of readdirSync(bundleDir)) {
      const full = path.join(bundleDir, name);
      if (statSync(full).isFile() && !name.endsWith(".json")) targets.push(full);
    }
  }
  const signed = [];
  for (const target of targets.sort()) {
    chmodSync(target, 0o755);
    const args = [
      "--force",
      "--options",
      "runtime",
      ...(identity === "-" ? [] : ["--timestamp"]),
      "--sign",
      identity,
      target,
    ];
    const result = spawnSync("codesign", args, { stdio: "inherit", env: process.env });
    if (result.error) fail(`codesign 无法执行：${result.error.message}`);
    if (result.status !== 0) fail(`codesign 失败（exit=${result.status}）：${target}`);
    signed.push(path.relative(resources, target));
    console.log(`[after-pack] 嵌套签名 ${path.relative(resources, target)}（${identity}）`);
  }
  // 顺序证据：嵌套二进制先于 `.app`（electron-builder 随后签名）。
  console.log(
    `[after-pack] 嵌套签名完成 ${signed.length} 个（顺序：bin/runtime-bundle → .app → DMG）`,
  );
  return signed;
}

module.exports = async function afterPack(context) {
  const platform = context.electronPlatformName;
  const productFilename = context.packager.appInfo.productFilename;
  const variant = process.env.AETHER_BUILD_VARIANT === "test" ? "test" : "release";
  const resources = resourcesDir(context.appOutDir, productFilename, platform);

  if (!existsSync(resources)) fail(`资源目录不存在：${resources}`);
  // Electron 发行包遗留的 `default_app.asar`（仅无 app 时使用）；打包后无用途。
  const defaultApp = path.join(resources, "default_app.asar");
  if (existsSync(defaultApp)) {
    rmSync(defaultApp, { force: true });
    console.log("[after-pack] 移除 Electron 遗留 default_app.asar（无用途）");
  }
  const daemon = path.join(resources, "bin", platform === "win32" ? "aetherd.exe" : "aetherd");
  if (!existsSync(daemon)) fail(`extraResources 缺少 daemon：${daemon}`);

  const runtimeIds = assertRuntimeBundle(resources, variant);
  const asarEntries = await assertAsar(resources);

  // 翻转 fuses（打包产物；签名前）。
  const appBinaryPath = appBinary(context.appOutDir, productFilename, platform);
  if (!existsSync(appBinaryPath)) fail(`找不到打包后的应用二进制：${appBinaryPath}`);
  runNode([path.join(REPO_ROOT, "scripts", "ci", "electron-fuses.mjs"), "apply", appBinaryPath]);

  let nestedSigned = [];
  if (platform === "darwin") {
    nestedSigned = signMacNested(resources);
  }

  console.log(
    `[after-pack] variant=${variant} platform=${platform} arch=${context.arch} ` +
      `runtimes=${runtimeIds.join(",")} asarEntries=${asarEntries} nestedSigned=${nestedSigned.length}`,
  );
};
