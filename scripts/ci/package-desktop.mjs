#!/usr/bin/env node
/**
 * `pnpm package:desktop`（M9-11；ADR-029 v0.4 §8.1 / D-029-10）。
 *
 * 打包链（顺序不可交换）：
 *   1. `build-runtime-bundles.mjs`：3 官方适配器 + `runtimes.json` + `digests.json`
 *      （`--variant test` 时额外含 Mock，仅测试构建，ADR-008）；
 *   2. `cargo build -p aether-daemon --release`，并以 `AETHER_RUNTIME_BUNDLE_DIR`
 *      把**同一目录**的清单/程序摘要编译进 daemon（ADR-015 §2.2#2，信任锚）；
 *   3. `pnpm --filter @aether/desktop build`（renderer + main/preload）；
 *   4. electron-builder：MSI（Windows）/ per-arch DMG（macOS）/ deb+AppImage（Linux），
 *      `afterPack` 翻转 fuses、断言 asar/sidecar、macOS 嵌套签名（先 sidecar 后 .app）；
 *   5. 断言打包后 fuse 线、产物哈希、体积硬阈值，写 `dist-out` 清单。
 *
 * 用法：
 *   node scripts/ci/package-desktop.mjs [--variant release|test] [--arch x64|arm64]
 *     [--dir] [--skip-build] [--skip-app-build] [--out <dir>] [--linux] [--win] [--mac]
 *
 * 说明：
 * - 跨架构（arm64）需要对应 Rust target 与 Electron 发行目录（`AETHER_ELECTRON_DIST`）；
 *   本机未安装 target 时脚本明确失败，不静默降级。
 * - Linux 仅构建（不纳入签名发布门禁，ADR-029 §8.1）；CI 冒烟由 M9-12 接入。
 * - 无证书时构建继续（空签），但 `verify:m9-11`/G6 会把签名缺失判为失败；
 *   `AETHER_REQUIRE_SIGNING=1` 可在发布流水线让构建直接失败。
 */
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run } from "../test/lib/exec.mjs";
import { listAsarFiles, asarContainsBasename } from "./asar.mjs";

function argValue(flag, fallback) {
  const index = process.argv.indexOf(flag);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

function flag(name) {
  return process.argv.includes(name);
}

const variant = argValue("--variant", process.env.AETHER_BUILD_VARIANT === "test" ? "test" : "release");
if (variant !== "release" && variant !== "test") {
  console.error(`[package] 未知 --variant：${variant}（release|test）`);
  process.exit(2);
}
const hostArch = process.arch === "arm64" ? "arm64" : "x64";
const arch = argValue("--arch", hostArch);
if (arch !== "x64" && arch !== "arm64") {
  console.error(`[package] 未知 --arch：${arch}（x64|arm64）`);
  process.exit(2);
}
const outDir = path.resolve(repoRoot, argValue("--out", "dist-out"));
const stageDir = path.join(repoRoot, "build", "stage", variant);
const binDir = path.join(stageDir, "bin");
const bundleDir = path.join(stageDir, "runtime-bundle");
const platform = flag("--linux") ? "linux" : flag("--mac") ? "darwin" : flag("--win") ? "win32" : process.platform;
const dirOnly = flag("--dir");
const skipBuild = flag("--skip-build");
const skipAppBuild = flag("--skip-app-build") || skipBuild;
const exe = platform === "win32" ? ".exe" : "";
const rustTarget =
  platform === "win32"
    ? arch === "arm64"
      ? "aarch64-pc-windows-msvc"
      : "x86_64-pc-windows-msvc"
    : platform === "darwin"
      ? arch === "arm64"
        ? "aarch64-apple-darwin"
        : "x86_64-apple-darwin"
      : arch === "arm64"
        ? "aarch64-unknown-linux-gnu"
        : "x86_64-unknown-linux-gnu";
const crossArch = arch !== hostArch || platform !== process.platform;
const packageJson = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
const version = packageJson.version;
const thresholds = JSON.parse(
  readFileSync(path.join(repoRoot, "docs", "M9-01-baseline", "freeze", "thresholds.json"), "utf8"),
);

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function ensure(condition, message) {
  if (!condition) {
    console.error(`[package] FAIL ${message}`);
    process.exit(1);
  }
}

function stagingBundleIsUsable() {
  const manifestPath = path.join(bundleDir, "runtimes.json");
  const digestsPath = path.join(bundleDir, "digests.json");
  if (!existsSync(manifestPath) || !existsSync(digestsPath)) return false;
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const ids = (manifest.runtimes ?? []).map((entry) => entry.id);
  if (!["claude-code", "codex", "deepseek-harness"].every((id) => ids.includes(id))) return false;
  if (variant === "test" && !ids.includes("mock")) return false;
  if (variant === "release" && ids.includes("mock")) return false;
  return (manifest.runtimes ?? []).every((entry) =>
    existsSync(path.join(bundleDir, entry.program)),
  );
}

function buildBundle() {
  rmSync(bundleDir, { recursive: true, force: true });
  mkdirSync(bundleDir, { recursive: true });
  const args = [path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs"), "--out", bundleDir];
  const code = run(process.execPath, args, {
    env: { ...process.env, ...(variant === "test" ? { AETHER_BUNDLE_MOCK: "1" } : {}) },
  });
  ensure(code === 0, "运行时适配器构建失败");
}

function buildDaemon() {
  const useTarget = crossArch || platform !== process.platform;
  const args = ["build", "-p", "aether-daemon", "--release"];
  if (useTarget) args.push("--target", rustTarget);
  const code = run(bin("cargo"), args, {
    env: {
      ...process.env,
      // 信任锚：与 extraResources 分发的 runtime-bundle 必须同源。
      AETHER_RUNTIME_BUNDLE_DIR: bundleDir,
      CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
    },
  });
  ensure(code === 0, "aetherd release 构建失败");
  const output = useTarget
    ? path.join(repoRoot, "target", rustTarget, "release", `aetherd${exe}`)
    : path.join(repoRoot, "target", "release", `aetherd${exe}`);
  ensure(existsSync(output), `未找到 daemon 产物：${output}`);
  mkdirSync(binDir, { recursive: true });
  copyFileSync(output, path.join(binDir, `aetherd${exe}`));
  console.log(`[package] 暂存 daemon：${path.relative(repoRoot, path.join(binDir, `aetherd${exe}`))}`);
}

function buildApp() {
  const { command: pnpm, prefix } = pnpmCommand();
  const code = run(pnpm, [...prefix, "--filter", "@aether/desktop", "build"]);
  ensure(code === 0, "Electron 应用构建失败");
}

function electronDistForArch() {
  // 本机 node_modules 的 Electron 发行目录只含宿主架构；跨架构交给 electron-builder
  // 下载对应发行（CI 可预置 AETHER_ELECTRON_DIST）。
  if (process.env.AETHER_ELECTRON_DIST) return path.resolve(process.env.AETHER_ELECTRON_DIST);
  if (arch === hostArch && platform === process.platform) {
    const dist = path.join(repoRoot, "apps", "desktop", "node_modules", "electron", "dist");
    if (!existsSync(dist)) {
      // CI 上 Electron 的 postinstall 可能未落 dist（或缓存未恢复）；交给
      // electron-builder 下载对应平台发行包。
      console.log(`[package] 未找到本机 Electron dist（${dist}）：由 electron-builder 下载`);
      return null;
    }
    // M9-07 开发/E2E 会在 Electron 发行目录创建 `Aether.exe` 硬链接（单实例命名管道
    // 需要产品名）。打包必须只保留 `electron.exe`，否则该 250MB 副本会被一并装入 MSI。
    const stray = path.join(dist, "Aether.exe");
    if (existsSync(stray)) {
      rmSync(stray, { force: true });
      console.log("[package] 已移除 Electron 发行目录中的开发期 Aether.exe 硬链接");
    }
    return dist;
  }
  return null;
}

function electronBuilderTargets() {
  if (platform === "win32") return ["--win", "msi"];
  if (platform === "darwin") return ["--mac", "dmg"];
  return ["--linux", "deb", "AppImage"];
}

function runElectronBuilder() {
  const cli = path.join(repoRoot, "node_modules", "electron-builder", "out", "cli", "cli.js");
  ensure(existsSync(cli), `缺少 electron-builder CLI：${cli}`);
  const electronDist = electronDistForArch();
  const args = [
    cli,
    "--config",
    path.join(repoRoot, "apps", "desktop", "electron-builder.config.mjs"),
    "--publish",
    "never",
    ...electronBuilderTargets(),
    ...(dirOnly ? ["--dir"] : []),
    arch === "arm64" ? "--arm64" : "--x64",
  ];
  mkdirSync(outDir, { recursive: true });
  const code = run(process.execPath, args, {
    env: {
      ...process.env,
      AETHER_BUILD_VARIANT: variant,
      AETHER_STAGE_DIR: stageDir,
      ...(electronDist ? { AETHER_ELECTRON_DIST: electronDist } : {}),
    },
  });
  ensure(code === 0, "electron-builder 打包失败");
}

function unpackedAppBinary() {
  const candidates =
    platform === "win32"
      ? [
          path.join(outDir, "win-unpacked", "Aether.exe"),
          path.join(outDir, "win-arm64-unpacked", "Aether.exe"),
        ]
      : platform === "darwin"
        ? [
            path.join(outDir, "mac-arm64", "Aether.app", "Contents", "MacOS", "Aether"),
            path.join(outDir, "mac", "Aether.app", "Contents", "MacOS", "Aether"),
            path.join(outDir, "mac-x64", "Aether.app", "Contents", "MacOS", "Aether"),
            path.join(outDir, "mac-universal", "Aether.app", "Contents", "MacOS", "Aether"),
          ]
        : [
            path.join(outDir, "linux-unpacked", "aether"),
            path.join(outDir, "linux-arm64-unpacked", "aether"),
          ];
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

function unpackedResourcesDir() {
  const binary = unpackedAppBinary();
  if (binary === null) return null;
  return platform === "darwin"
    ? path.join(path.dirname(binary), "..", "Resources")
    : path.join(path.dirname(binary), "resources");
}

function assertUnpacked() {
  const binary = unpackedAppBinary();
  ensure(binary !== null, "未找到解包后的应用二进制（electron-builder 输出目录异常）");
  const fuses = run(process.execPath, [
    path.join(repoRoot, "scripts", "ci", "electron-fuses.mjs"),
    "assert",
    binary,
  ]);
  ensure(fuses === 0, "打包产物 fuses 断言失败");

  const resources = unpackedResourcesDir();
  ensure(resources !== null && existsSync(resources), "未找到打包资源目录");
  const daemon = path.join(resources, "bin", `aetherd${exe}`);
  ensure(existsSync(daemon), `extraResources 缺少 daemon：${daemon}`);
  const manifestPath = path.join(resources, "runtime-bundle", "runtimes.json");
  ensure(existsSync(manifestPath), `extraResources 缺少 runtime-bundle：${manifestPath}`);
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const ids = (manifest.runtimes ?? []).map((entry) => entry.id);
  ensure(
    ["claude-code", "codex", "deepseek-harness"].every((id) => ids.includes(id)),
    `runtime-bundle 官方运行时缺失：${ids.join(",")}`,
  );
  if (variant === "release") ensure(!ids.includes("mock"), "release 产物含 Mock 适配器");
  if (variant === "test") ensure(ids.includes("mock"), "test 产物缺少 Mock 适配器");

  const asarPath = path.join(resources, "app.asar");
  ensure(existsSync(asarPath), `缺少 app.asar：${asarPath}`);
  const files = listAsarFiles(asarPath);
  for (const forbidden of [
    "aetherd",
    "aetherd.exe",
    "aether-claude-adapter",
    "aether-codex-adapter",
    "aether-dsh-adapter",
    "aether-mock-adapter",
  ]) {
    ensure(!asarContainsBasename(files, forbidden), `asar 内出现 sidecar：${forbidden}`);
  }
  ensure(
    files.includes("/apps/desktop/dist/index.html"),
    "asar 缺少 renderer 入口 apps/desktop/dist/index.html",
  );
  ensure(
    files.includes("/apps/desktop/electron/dist/main.cjs"),
    "asar 缺少 main 入口 apps/desktop/electron/dist/main.cjs",
  );
  ensure(
    existsSync(
      path.join(
        resources,
        "app.asar.unpacked",
        "apps",
        "desktop",
        "electron",
        "dist",
        "shell",
        "startup",
        "startup.html",
      ),
    ),
    "壳级页面未解包（asarUnpack；file:// 不读 asar）",
  );
  console.log(
    `[package] 解包断言通过：binary=${path.relative(repoRoot, binary)} asarEntries=${files.length} runtimes=${ids.join(",")}`,
  );
  return { binary, resources, asarEntries: files.length, runtimeIds: ids };
}

function artifactsOf(extension) {
  if (!existsSync(outDir)) return [];
  const prefix =
    variant === "test" ? `Aether-${version}-test-${arch}.` : `Aether-${version}-${arch}.`;
  return readdirSync(outDir)
    .filter((name) => name.startsWith(prefix) && name.endsWith(extension))
    .map((name) => path.join(outDir, name))
    .filter((file) => statSync(file).isFile());
}

function writeManifests(unpacked) {
  const extensions = dirOnly
    ? []
    : platform === "win32"
      ? [".msi"]
      : platform === "darwin"
        ? [".dmg"]
        : [".deb", ".AppImage"];
  const artifacts = extensions.flatMap((extension) => artifactsOf(extension));
  ensure(dirOnly || artifacts.length > 0, "未找到打包产物（MSI/DMG/deb/AppImage）");

  const artifactEntries = artifacts.map((file) => {
    const bytes = statSync(file).size;
    return {
      name: path.basename(file),
      path: path.relative(repoRoot, file),
      bytes,
      sha256: sha256(file),
      size_pass: bytes <= thresholds.installer.max_bytes,
    };
  });
  const sumsName = variant === "test" ? "SHA256SUMS-test.txt" : "SHA256SUMS.txt";
  if (artifactEntries.length > 0) {
    const sums = artifactEntries
      .map((entry) => `${entry.sha256}  ${entry.name}`)
      .join("\n");
    writeFileSync(path.join(outDir, sumsName), `${sums}\n`, "utf8");
  }
  const builtAt = new Date().toISOString();
  const commit = runCapture("git", ["rev-parse", "HEAD"]);
  const manifest = {
    schema: "aether.m9-11.build-manifest.v1",
    variant,
    version,
    platform: platform === "win32" ? "windows" : platform === "darwin" ? "macos" : "linux",
    arch,
    rust_target: rustTarget,
    built_at: builtAt,
    commit: commit.trim(),
    electron_version: JSON.parse(
      readFileSync(path.join(repoRoot, "apps", "desktop", "node_modules", "electron", "package.json"), "utf8"),
    ).version,
    node_version: process.version,
    dir_only: dirOnly,
    artifacts: artifactEntries,
    unpacked: unpacked
      ? {
          binary: path.relative(repoRoot, unpacked.binary),
          asar_entries: unpacked.asarEntries,
          runtime_ids: unpacked.runtimeIds,
        }
      : null,
    thresholds: {
      installer_max_bytes: thresholds.installer.max_bytes,
      installer_rule: thresholds.installer.rule,
      absolute_cap_bytes: thresholds.installer.absolute_cap_bytes,
    },
    size_pass: artifactEntries.length === 0 ? null : artifactEntries.every((entry) => entry.size_pass),
    signing: {
      windows_cert_configured: Boolean(process.env.CSC_LINK || process.env.AETHER_WIN_CERT_FILE),
      mac_identity_configured: Boolean(process.env.CSC_NAME || process.env.AETHER_MAC_IDENTITY),
      notarize: process.env.AETHER_NOTARIZE === "1",
      require_signing: process.env.AETHER_REQUIRE_SIGNING === "1",
      // ADR-030：Windows 允许无证书发布；该字段随证据归档（macOS 仍强制）。
      unsigned_release:
        platform === "win32" &&
        variant === "release" &&
        !(process.env.CSC_LINK || process.env.CSC_NAME || process.env.AETHER_WIN_CERT_FILE),
    },
    runtime_bundle: {
      dir: path.relative(repoRoot, bundleDir),
      manifest: JSON.parse(readFileSync(path.join(bundleDir, "runtimes.json"), "utf8")),
      digests: JSON.parse(readFileSync(path.join(bundleDir, "digests.json"), "utf8")),
    },
  };
  const manifestName =
    variant === "test" ? "m9-11-build-manifest-test.json" : "m9-11-build-manifest.json";
  writeFileSync(path.join(outDir, manifestName), `${JSON.stringify(manifest, null, 2)}\n`, "utf8");

  for (const entry of artifactEntries) {
    console.log(
      `[package] 产物 ${entry.name}：${entry.bytes} bytes sha256=${entry.sha256}` +
        (entry.size_pass
          ? "（体积 ≤ 硬阈值）"
          : `（超硬阈值 ${thresholds.installer.max_bytes} bytes → G6 不通过）`),
    );
  }
  console.log(`[package] 清单：${path.relative(repoRoot, path.join(outDir, manifestName))}`);
  if (variant === "release" && artifactEntries.some((entry) => !entry.size_pass)) {
    console.error("[package] 体积硬阈值失败（G6 不通过；不得以评审豁免，ADR-029 §15-10）");
    process.exitCode = 1;
  }
}

function runCapture(name, args) {
  const result = spawnSync(name, args, { cwd: repoRoot, encoding: "utf8" });
  return result.status === 0 ? `${result.stdout ?? ""}` : "";
}

// ===== 主流程 =====
if (platform !== process.platform && !skipBuild) {
  console.error(
    `[package] 跨平台打包未在本任务验证（目标 ${platform}，宿主 ${process.platform}）；` +
      "请在对应平台/CI runner 执行（M9-12）。",
  );
  process.exit(2);
}

rmSync(path.join(repoRoot, "build", "resources"), { recursive: true, force: true });
mkdirSync(path.join(repoRoot, "build", "resources"), { recursive: true });

if (skipBuild) {
  ensure(stagingBundleIsUsable(), `--skip-build 但暂存 bundle 不可用：${bundleDir}`);
  ensure(existsSync(path.join(binDir, `aetherd${exe}`)), `--skip-build 但缺少 daemon：${binDir}`);
  console.log("[package] --skip-build：复用暂存 daemon/runtime-bundle");
} else {
  buildBundle();
  buildDaemon();
}
if (!skipAppBuild) buildApp();
else console.log("[package] 跳过应用构建（--skip-app-build/--skip-build）");

runElectronBuilder();
const unpacked = assertUnpacked();
writeManifests(unpacked);

console.log(
  `[package] 完成 variant=${variant} platform=${platform} arch=${arch} out=${path.relative(repoRoot, outDir)}`,
);
