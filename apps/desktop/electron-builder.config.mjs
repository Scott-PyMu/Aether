/**
 * electron-builder 打包配置（M9-11；ADR-029 v0.4 §8.1 / D-029-10）。
 *
 * 关键口径：
 * - 应用入口 = 仓库根 `package.json` 的 `main`（`apps/desktop/electron/dist/main.cjs`），
 *   因此 `files` 必须保留 `apps/desktop/{dist,electron/dist}` 的目录结构：
 *   `app.getAppPath()` 在 asar 内为 `resources/app.asar`，main 侧
 *   `rendererDistDir()` 按 `<appPath>/apps/desktop/dist` 解析 renderer。
 * - `aetherd` 与 `runtime-bundle/` 走 `extraResources`（不进 asar）；
 *   daemon 侧按 `<resources>/bin/aetherd` + `<resources>/runtime-bundle` 解析。
 * - Windows MSI：per-user、`asInvoker`；macOS：per-arch DMG（arm64/x64，不做 universal）、
 *   hardened runtime、嵌套签名顺序由 `scripts/ci/electron-after-pack.cjs` 先签
 *   `aetherd`/适配器，再由 electron-builder 签 `.app`/DMG。
 * - 无自动更新器（P3 另立 ADR）；`--publish never` 由打包脚本强制。
 *
 * 环境变量（均由 `scripts/ci/package-desktop.mjs` 注入；直接调用 electron-builder 时
 * 使用缺省 release 布局）：
 * - `AETHER_BUILD_VARIANT`：`release`（缺省）| `test`（含 Mock 适配器，仅测试构建）；
 * - `AETHER_STAGE_DIR`：`bin/` + `runtime-bundle/` 暂存目录；
 * - `AETHER_ELECTRON_DIST`：跨架构打包时的 Electron 发行目录（缺省本机 node_modules）；
 * - `AETHER_NOTARIZE=1`：启用 macOS 公证（凭据走 APPLE_ID/APPLE_APP_SPECIFIC_PASSWORD/APPLE_TEAM_ID）；
 * - `AETHER_REQUIRE_SIGNING=1`：无证书时构建失败（G6 发布门禁用）。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(appDir, "..", "..");
const variant = process.env.AETHER_BUILD_VARIANT === "test" ? "test" : "release";
const packageJson = JSON.parse(
  readFileSync(path.join(repoRoot, "package.json"), "utf8"),
);
const electronVersion = JSON.parse(
  readFileSync(path.join(appDir, "node_modules", "electron", "package.json"), "utf8"),
).version;
const stageDir = path.resolve(
  process.env.AETHER_STAGE_DIR ?? path.join(repoRoot, "build", "stage", variant),
);
const iconDir = path.join(repoRoot, "crates", "aether-tauri", "icons");
const entitlements = path.join(appDir, "electron", "entitlements.mac.plist");
const requireSigning = process.env.AETHER_REQUIRE_SIGNING === "1";
const notarize = process.env.AETHER_NOTARIZE === "1";
const macSigningConfigured = Boolean(
  process.env.CSC_LINK || process.env.CSC_NAME || process.env.AETHER_MAC_IDENTITY,
);

/** @type {import("electron-builder").Configuration} */
export default {
  appId: "dev.aether.desktop",
  productName: "Aether",
  copyright: `Copyright © 2026 Aether`,
  // 应用目录显式固定为仓库根：保留 `apps/desktop/...` 路径（见文件头）。
  directories: {
    app: repoRoot,
    output: path.join(repoRoot, "dist-out"),
    buildResources: path.join(repoRoot, "build", "resources"),
  },
  asar: true,
  // 体积硬阈值（≤1.5×HEAD 且 ≤450MB）：使用最高压缩（构建时间换包体，G6 门禁）。
  compression: "maximum",
  electronVersion,
  ...(process.env.AETHER_ELECTRON_DIST
    ? { electronDist: path.resolve(process.env.AETHER_ELECTRON_DIST) }
    : {}),
  npmRebuild: false,
  buildDependenciesFromSource: false,
  nodeGypRebuild: false,
  removePackageScripts: true,
  // MSI/manifest 元数据（应用 package.json 只承载构建脚本）。
  extraMetadata: {
    author: "Aether",
    description: "Aether multi-agent orchestration desktop",
  },
  // 仅保留中英文 Chromium locale（减小包体；UI 文案为 zh-CN，英文为回退）。
  electronLanguages: ["en-US", "zh-CN"],
  // 不使用远程更新器（ADR-029 §5.5）。
  publish: null,
  artifactName:
    variant === "test"
      ? "${productName}-${version}-test-${arch}.${ext}"
      : "${productName}-${version}-${arch}.${ext}",
  files: [
    // renderer：`<appPath>/apps/desktop/dist`
    "apps/desktop/dist/**/*",
    // main/preload/壳级页面：`<appPath>/apps/desktop/electron/dist`
    "apps/desktop/electron/dist/**/*",
    "package.json",
  ],
  // `BrowserWindow.loadFile()` 经 `file://` 无法读取 asar 内页面（Electron 45 实测：
  // ERR_FILE_NOT_FOUND）；壳级页面（S-00/S-12/S-13）解包到 `app.asar.unpacked`。
  asarUnpack: ["apps/desktop/electron/dist/shell/**"],
  extraResources: [
    // `<resources>/bin/aetherd(.exe)`
    { from: path.join(stageDir, "bin"), to: "bin" },
    // `<resources>/runtime-bundle/`（3 官方适配器 + runtimes.json + digests.json）
    { from: path.join(stageDir, "runtime-bundle"), to: "runtime-bundle" },
  ],
  afterPack: path.join(repoRoot, "scripts", "ci", "electron-after-pack.cjs"),
  win: {
    target: "msi",
    icon: path.join(iconDir, "icon.ico"),
    // ADR-029 §8.1：per-user 安装、asInvoker（不提权）。
    requestedExecutionLevel: "asInvoker",
    signAndEditExecutable: true,
    forceCodeSigning: requireSigning,
    ...(process.env.AETHER_WIN_CERT_FILE
      ? {
          signtoolOptions: {
            certificateFile: path.resolve(process.env.AETHER_WIN_CERT_FILE),
            certificatePassword: process.env.AETHER_WIN_CERT_PASSWORD ?? null,
            // RFC3161 时间戳（签名链保留时间证明）。
            rfc3161TimeStampServer:
              process.env.AETHER_WIN_TIMESTAMP_URL ?? "http://timestamp.digicert.com",
          },
        }
      : {}),
  },
  msi: {
    // per-user 安装；oneClick=false 让安装目录使用产品名 `Aether`（electron-builder
    // 在 oneClick=true 且非 per-machine 时会退化为 package name），静默安装仍由
    // `msiexec /qn` 驱动，卸载不删除数据目录。
    oneClick: false,
    perMachine: false,
    runAfterFinish: false,
    createDesktopShortcut: true,
    createStartMenuShortcut: true,
    shortcutName: "Aether",
  },
  mac: {
    // per-arch DMG（arm64/x64），不做 universal（P2-6）。
    target: "dmg",
    category: "public.app-category.developer-tools",
    icon: path.join(iconDir, "icon.icns"),
    minimumSystemVersion: "12.0",
    hardenedRuntime: true,
    gatekeeperAssess: false,
    entitlements,
    entitlementsInherit: entitlements,
    forceCodeSigning: requireSigning,
    ...(process.env.AETHER_MAC_IDENTITY
      ? { identity: process.env.AETHER_MAC_IDENTITY }
      : {}),
    notarize,
  },
  dmg: {
    // DMG 需在 `.app` 签名后签名；无签名身份时保持未签（G6 由签名校验脚本判失败）。
    sign: macSigningConfigured,
    title: `Aether ${packageJson.version}`,
  },
  linux: {
    target: ["deb", "AppImage"],
    category: "Development",
    maintainer: "Aether",
    vendor: "Aether",
    synopsis: "Aether multi-agent orchestration desktop",
    executableName: "aether",
    icon: path.join(iconDir, "icon.png"),
  },
};
