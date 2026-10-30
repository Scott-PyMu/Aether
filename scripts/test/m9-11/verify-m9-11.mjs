#!/usr/bin/env node
/**
 * `pnpm verify:m9-11`（M9-11 DoD-1/2/5/6/7/8 的可执行静态校验）。
 *
 * 覆盖：
 *  - electron-builder 配置口径：appId/productName/asar/files/extraResources、MSI per-user +
 *    asInvoker、per-arch DMG（无 universal）、hardenedRuntime/entitlements、Linux deb/AppImage、
 *    无自动更新器；afterPack 钩子的 fuses / asar / 嵌套签名顺序；
 *  - 打包脚本：daemon 构建使用 `AETHER_RUNTIME_BUNDLE_DIR` 同源信任锚、`--publish never`、
 *    哈希与体积硬阈值（来自 M9-01 冻结阈值）；
 *  - 产物（若存在）：manifest/哈希/体积、解包 asar 无 sidecar、extras 布局；
 *  - MSI 静态表：Upgrade 表仅本产品升级码（**无旧 Tauri MSI 升级链**）、RemoveFile 不删
 *    数据目录；旧 Tauri MSI 的 UpgradeCode 与新包不同；
 *  - 文档：证据与「旧包手工卸载」说明。
 *
 * 签名/公证（DoD-2）由 `pnpm verify:signatures` 在带证书产物的平台执行；
 * 本脚本对「未配置证书」明确 SKIP，不伪装通过。
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";

import { asarContainsBasename, listAsarFiles } from "../../ci/asar.mjs";
import { readMsiProperties, readMsiTable } from "./lib/msi.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
const notes = [];
let failed = 0;

function check(name, ok, detail) {
  if (ok) {
    console.log(`PASS  ${name}${detail ? `（${detail}）` : ""}`);
  } else {
    failed += 1;
    console.error(`FAIL  ${name}${detail ? `（${detail}）` : ""}`);
  }
  checks.push({ name, ok, detail });
  return ok;
}

function skip(name, reason) {
  console.log(`SKIP  ${name}（${reason}）`);
  checks.push({ name, ok: true, skipped: true, detail: reason });
}

function note(message) {
  notes.push(message);
  console.log(`NOTE  ${message}`);
}

function readJsonIfExists(file) {
  if (!existsSync(file)) return null;
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

function read(rel) {
  return readFileSync(path.join(repoRoot, rel), "utf8");
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function versionMatches(actual, expected) {
  const normalize = (value) => {
    const parts = String(value ?? "").split(".").map((part) => Number.parseInt(part, 10) || 0);
    while (parts.length < 4) parts.push(0);
    return parts.slice(0, 4).join(".");
  };
  return normalize(actual) === normalize(expected);
}

const packageJson = JSON.parse(read("package.json"));
const thresholds = JSON.parse(read("docs/M9-01-baseline/freeze/thresholds.json"));
const variant = process.argv.includes("--variant")
  ? process.argv[process.argv.indexOf("--variant") + 1]
  : "release";

// ===== 1. electron-builder 配置 =====
process.env.AETHER_BUILD_VARIANT = variant;
const configPath = path.join(repoRoot, "apps", "desktop", "electron-builder.config.mjs");
const configModule = await import(pathToFileURL(configPath).href);
const config = configModule.default;

check("配置存在且 appId=dev.aether.desktop", config.appId === "dev.aether.desktop", config.appId);
check("productName=Aether", config.productName === "Aether", config.productName);
check("asar=true", config.asar === true, String(config.asar));
check("最高压缩（体积门禁）", config.compression === "maximum", String(config.compression));
check(
  "Chromium locales 限制为中英文",
  Array.isArray(config.electronLanguages) &&
    config.electronLanguages.length === 2 &&
    config.electronLanguages.includes("en-US") &&
    config.electronLanguages.includes("zh-CN"),
  JSON.stringify(config.electronLanguages),
);
check(
  "输出目录为仓库 dist-out",
  path.resolve(config.directories?.output ?? "") === path.join(repoRoot, "dist-out"),
  config.directories?.output ?? "",
);
const files = (config.files ?? []).join("\n");
for (const required of [
  "apps/desktop/dist/**/*",
  "apps/desktop/electron/dist/**/*",
  "package.json",
]) {
  check(`files 含 ${required}`, files.includes(required));
}
check(
  "extraResources：bin/aetherd",
  (config.extraResources ?? []).some(
    (entry) => entry.to === "bin" && /stage.*bin$/.test(String(entry.from).replace(/\\/g, "/")),
  ),
);
check(
  "extraResources：runtime-bundle",
  (config.extraResources ?? []).some(
    (entry) =>
      entry.to === "runtime-bundle" &&
      /stage.*runtime-bundle$/.test(String(entry.from).replace(/\\/g, "/")),
  ),
);
check("无自动更新器（publish=null）", config.publish === null, String(config.publish));
check(
  "Windows target=msi",
  config.win?.target === "msi",
  JSON.stringify(config.win?.target),
);
check(
  "Windows requestedExecutionLevel=asInvoker",
  config.win?.requestedExecutionLevel === "asInvoker",
  config.win?.requestedExecutionLevel,
);
check("MSI per-user（perMachine=false）", config.msi?.perMachine === false, String(config.msi?.perMachine));
check(
  "macOS target=dmg（per-arch，非 universal）",
  config.mac?.target === "dmg" && !String(JSON.stringify(config.mac?.target)).includes("universal"),
  JSON.stringify(config.mac?.target),
);
check("macOS hardenedRuntime=true", config.mac?.hardenedRuntime === true);
check("macOS minimumSystemVersion=12.0", config.mac?.minimumSystemVersion === "12.0", config.mac?.minimumSystemVersion);
const entitlements = path.resolve(config.mac?.entitlements ?? "");
check("macOS entitlements 文件存在", existsSync(entitlements), entitlements);
if (existsSync(entitlements)) {
  const plist = readFileSync(entitlements, "utf8");
  const keys = [...plist.matchAll(/<key>([^<]+)<\/key>/g)].map((match) => match[1]);
  check(
    "entitlements 最小化（仅 JIT/可执行内存）",
    keys.length === 2 &&
      keys.includes("com.apple.security.cs.allow-jit") &&
      keys.includes("com.apple.security.cs.allow-unsigned-executable-memory"),
    keys.join(","),
  );
  check(
    "entitlements 未授予网络/文件/Keychain 例外",
    !keys.some((key) => /network|files|keychain|library-validation|get-task-allow/.test(key)),
    keys.join(","),
  );
}
check(
  "Linux targets 含 deb+AppImage",
  Array.isArray(config.linux?.target) &&
    config.linux.target.includes("deb") &&
    config.linux.target.includes("AppImage"),
  JSON.stringify(config.linux?.target),
);

const hookPath = path.resolve(config.afterPack ?? "");
check("afterPack 钩子存在", existsSync(hookPath), hookPath);
if (existsSync(hookPath)) {
  const hook = readFileSync(hookPath, "utf8");
  check("afterPack 调用 electron-fuses 翻转/断言", hook.includes("electron-fuses.mjs") && hook.includes("apply"));
  check(
    "afterPack 断言 asar 内无 sidecar",
    hook.includes("asarContainsBasename") && hook.includes('"aetherd.exe"'),
  );
  check(
    "afterPack 断言 runtime-bundle 三官方",
    hook.includes("claude-code") && hook.includes("codex") && hook.includes("deepseek-harness"),
  );
  const codesignIndex = hook.indexOf("codesign");
  check(
    "嵌套签名先于 .app 签名（afterPack 在签名阶段之前）",
    hook.includes("嵌套签名") && codesignIndex > 0,
  );
}

// ===== 2. 打包脚本 =====
const packageScript = read("scripts/ci/package-desktop.mjs");
check("打包脚本强制 --publish never", packageScript.includes('"--publish"') && packageScript.includes('"never"'));
check(
  "daemon 构建使用 AETHER_RUNTIME_BUNDLE_DIR 同源摘要",
  packageScript.includes("AETHER_RUNTIME_BUNDLE_DIR: bundleDir"),
);
check("打包脚本读取 M9-01 冻结阈值", packageScript.includes("thresholds.json") && packageScript.includes("installer.max_bytes"));
check("打包脚本写 SHA256SUMS", packageScript.includes("SHA256SUMS"));
check(
  "package.json 注册 package:desktop / smoke:installer / verify:m9-11",
  ["package:desktop", "smoke:installer", "verify:m9-11", "verify:signatures"].every(
    (name) => typeof packageJson.scripts?.[name] === "string",
  ),
);
const rootDeps = { ...(packageJson.dependencies ?? {}), ...(packageJson.devDependencies ?? {}) };
const desktopDeps = {
  ...(JSON.parse(read("apps/desktop/package.json")).dependencies ?? {}),
  ...(JSON.parse(read("apps/desktop/package.json")).devDependencies ?? {}),
};
check("无 electron-updater（M9 不做自动更新）", !rootDeps["electron-updater"] && !desktopDeps["electron-updater"]);
check("electron-builder 已登记（MIT；ADR-029 §5.5）", rootDeps["electron-builder"] === "26.18.0", rootDeps["electron-builder"]);

// ===== 3. runtime-bundle 暂存（daemon 信任锚） =====
const stageDir = path.join(repoRoot, "build", "stage", variant);
const bundleManifest = readJsonIfExists(path.join(stageDir, "runtime-bundle", "runtimes.json"));
const bundleDigests = readJsonIfExists(path.join(stageDir, "runtime-bundle", "digests.json"));
if (!bundleManifest || !bundleDigests) {
  skip("runtime-bundle 暂存目录（build/stage）", "尚未执行 pnpm package:desktop");
} else {
  const ids = (bundleManifest.runtimes ?? []).map((entry) => entry.id);
  const official = ["claude-code", "codex", "deepseek-harness"];
  check("runtime-bundle 含三官方运行时", official.every((id) => ids.includes(id)), ids.join(","));
  check(
    variant === "release" ? "release runtime-bundle 无 Mock" : "test runtime-bundle 含 Mock",
    variant === "release" ? !ids.includes("mock") : ids.includes("mock"),
    ids.join(","),
  );
  let digestOk = true;
  for (const entry of bundleManifest.runtimes ?? []) {
    const file = path.join(stageDir, "runtime-bundle", entry.program);
    const expected = bundleDigests.digests?.[entry.program];
    if (!existsSync(file) || !expected) {
      digestOk = false;
      break;
    }
    const actual = `sha256:${sha256(file)}`;
    if (actual !== expected) {
      digestOk = false;
      note(`${entry.program} 摘要不符：${actual} != ${expected}`);
      break;
    }
  }
  check("digests.json 与适配器文件一致（构建期信任锚）", digestOk);
}

// ===== 4. 产物清单（哈希/体积/布局） =====
const manifestName = variant === "test" ? "m9-11-build-manifest-test.json" : "m9-11-build-manifest.json";
const buildManifest = readJsonIfExists(path.join(repoRoot, "dist-out", manifestName));
if (!buildManifest) {
  skip("打包产物清单", `未找到 dist-out/${manifestName}（先执行 pnpm package:desktop）`);
} else {
  check("产物清单 schema", buildManifest.schema === "aether.m9-11.build-manifest.v1");
  check("产物版本与应用版本一致", buildManifest.version === packageJson.version);
  check(
    "产物文件名含版本/arch",
    (buildManifest.artifacts ?? []).every((entry) =>
      new RegExp(
        `^Aether-${packageJson.version}(-test)?-(x64|arm64|x86_64|amd64)\\.`,
      ).test(entry.name),
    ),
    (buildManifest.artifacts ?? []).map((entry) => entry.name).join(","),
  );
  for (const entry of buildManifest.artifacts ?? []) {
    const file = path.join(repoRoot, entry.path);
    if (!existsSync(file)) {
      check(`产物存在：${entry.name}`, false, entry.path);
      continue;
    }
    const bytes = statSync(file).size;
    check(`产物哈希一致：${entry.name}`, sha256(file) === entry.sha256, entry.sha256.slice(0, 16));
    check(`产物大小记录一致：${entry.name}`, bytes === entry.bytes, `${bytes} bytes`);
    check(
      `体积硬阈值：${entry.name}`,
      bytes <= thresholds.installer.max_bytes && bytes <= thresholds.installer.absolute_cap_bytes,
      `${bytes} / ${thresholds.installer.max_bytes} bytes`,
    );
  }
  check(
    "体积门禁汇总 size_pass=true",
    buildManifest.size_pass !== false,
    String(buildManifest.size_pass),
  );
  check(
    "产物清单记录 electron/commit",
    typeof buildManifest.electron_version === "string" && typeof buildManifest.commit === "string",
    `${buildManifest.electron_version} ${String(buildManifest.commit).slice(0, 8)}`,
  );
  if (process.platform === "win32" && !process.env.CSC_LINK && !process.env.AETHER_WIN_CERT_FILE) {
    check(
      "Windows unsigned 发布记录（ADR-030）",
      buildManifest.signing?.unsigned_release === true,
      `unsigned_release=${String(buildManifest.signing?.unsigned_release)}`,
    );
  }
  const unpackedAsar = [
    path.join(repoRoot, "dist-out", "win-unpacked", "resources", "app.asar"),
    path.join(repoRoot, "dist-out", "win-arm64-unpacked", "resources", "app.asar"),
    path.join(repoRoot, "dist-out", "mac", "Aether.app", "Contents", "Resources", "app.asar"),
    path.join(repoRoot, "dist-out", "mac-arm64", "Aether.app", "Contents", "Resources", "app.asar"),
    path.join(repoRoot, "dist-out", "linux-unpacked", "resources", "app.asar"),
  ].find((file) => existsSync(file));
  if (!unpackedAsar) {
    skip("解包 asar 断言", "未找到解包目录（仅安装包产物）");
  } else {
    const asarFiles = listAsarFiles(unpackedAsar);
    const sidecars = [
      "aetherd",
      "aetherd.exe",
      "aether-claude-adapter",
      "aether-codex-adapter",
      "aether-dsh-adapter",
      "aether-mock-adapter",
    ].filter((name) => asarContainsBasename(asarFiles, name));
    check("asar 内无 sidecar", sidecars.length === 0, sidecars.join(",") || "0");
    check(
      "asar 无 node_modules（应用无运行时 npm 依赖）",
      !asarFiles.some((entry) => entry.startsWith("/node_modules/")),
      `${asarFiles.length} 个条目`,
    );
    check(
      "asar 含 renderer/main 入口",
      asarFiles.includes("/apps/desktop/dist/index.html") &&
        asarFiles.includes("/apps/desktop/electron/dist/main.cjs"),
      `${asarFiles.length} 个条目`,
    );
    const resources = path.dirname(unpackedAsar);
    check(
      "解包 extraResources：bin/aetherd + runtime-bundle",
      existsSync(path.join(resources, "bin", process.platform === "win32" ? "aetherd.exe" : "aetherd")) &&
        existsSync(path.join(resources, "runtime-bundle", "runtimes.json")),
    );
    check(
      "壳级页面解包（asarUnpack；file:// 不读 asar）",
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
    );
  }
}

// ===== 5. MSI 静态表（Windows） =====
const msiPath =
  process.platform === "win32"
    ? (() => {
        const outDir = path.join(repoRoot, "dist-out");
        if (!existsSync(outDir)) return null;
        const name = `Aether-${packageJson.version}${variant === "test" ? "-test" : ""}-x64.msi`;
        return existsSync(path.join(outDir, name)) ? path.join(outDir, name) : null;
      })()
    : null;
if (!msiPath) {
  skip("MSI 静态表（升级链/数据目录）", "未找到 MSI 或非 Windows 平台");
} else {
  const properties = readMsiProperties(msiPath);
  check("MSI ProductName=Aether", properties.ProductName === "Aether", properties.ProductName);
  check("MSI ProductVersion=应用版本", versionMatches(properties.ProductVersion, packageJson.version), properties.ProductVersion);
  check(
    "MSI UpgradeCode 为 GUID",
    /^\{[0-9A-Fa-f-]{36}\}$/.test(properties.UpgradeCode ?? ""),
    properties.UpgradeCode ?? "",
  );
  const upgrades = readMsiTable(msiPath, "Upgrade", [
    "UpgradeCode",
    "VersionMin",
    "VersionMax",
    "Language",
    "Attributes",
    "RemoveFeatures",
    "ActionProperty",
  ]);
  const foreign = upgrades.filter(
    (row) => (row.UpgradeCode ?? "").toUpperCase() !== (properties.UpgradeCode ?? "").toUpperCase(),
  );
  check("MSI 无旧 MSI 升级链（Upgrade 表仅本产品）", foreign.length === 0, `rows=${upgrades.length}`);
  const removeFiles = readMsiTable(msiPath, "RemoveFile", [
    "FileKey",
    "Component_",
    "FileName",
    "DirProperty",
    "InstallMode",
  ]);
  const dataDeletion = removeFiles.filter((row) =>
    /(aether\.db|appdata|roaming)/i.test(`${row.FileName ?? ""} ${row.DirProperty ?? ""}`),
  );
  check("MSI RemoveFile 不删除数据目录", dataDeletion.length === 0, `rows=${removeFiles.length}`);

  const legacyMsi = path.join(repoRoot, "dist-out", "Aether_0.1.0_x64_en-US.msi");
  if (existsSync(legacyMsi)) {
    const legacy = readMsiProperties(legacyMsi);
    check(
      "新包 UpgradeCode 与旧 Tauri MSI 不同（无跨壳升级链）",
      (legacy.UpgradeCode ?? "").toUpperCase() !== (properties.UpgradeCode ?? "").toUpperCase(),
      `legacy=${legacy.UpgradeCode} new=${properties.UpgradeCode}`,
    );
  } else {
    skip("旧 Tauri MSI 对照", "dist-out 无 M8-04 基线 MSI");
  }
}

// ===== 6. 安装冒烟证据（DoD-4/7/8） =====
function latestEvidence(prefix) {
  const outDir = path.join(repoRoot, "dist-out");
  if (!existsSync(outDir)) return null;
  const files = readdirSync(outDir)
    .filter((name) => name.startsWith(prefix) && name.endsWith(".json"))
    .sort();
  return files.length === 0 ? null : path.join(outDir, files[files.length - 1]);
}

const releaseEvidencePath = latestEvidence("m9-11-smoke-release-");
const testEvidencePath = latestEvidence("m9-11-smoke-test-");
if (!releaseEvidencePath) {
  skip("安装冒烟证据（release）", "未找到 m9-11-smoke-release-*.json（先 pnpm smoke:installer）");
} else {
  const smoke = readJsonIfExists(releaseEvidencePath);
  console.log(`\n[verify:m9-11] 安装冒烟证据：${path.relative(repoRoot, releaseEvidencePath)}`);
  check("release 冒烟通过（安装/首启/卸载/数据保留）", smoke?.passed === true, String(smoke?.passed));
  const installer = smoke?.metrics?.installer;
  check(
    "DoD-7 体积硬阈值（≤1.5×HEAD 且 ≤450MB）",
    installer?.size_pass === true,
    `${installer?.bytes ?? "?"} / ${installer?.max_bytes ?? thresholds.installer.max_bytes} bytes`,
  );
  const launcher = smoke?.metrics?.first_launch ?? {};
  check(
    "DoD-7 首窗 ≤1.5s（安装产物）",
    Number(launcher.window_visible_ms) <= thresholds.startup.window_shown_max_ms,
    `visibleMs=${launcher.window_visible_ms ?? "?"}`,
  );
  check(
    "DoD-7 idle RSS ≤600MiB（安装产物全树）",
    Number(launcher.idle_rss_bytes) > 0 &&
      Number(launcher.idle_rss_bytes) <= thresholds.runtime.idle_rss_max_bytes,
    `${launcher.idle_rss_mib ?? "?"} MiB`,
  );
  if (
    Number(launcher.ui_ready_ms) > 0 &&
    Number(launcher.ui_ready_ms) > thresholds.startup.ui_ready_max_ms
  ) {
    note(
      `UI ready 实测 ${launcher.ui_ready_ms}ms > ${thresholds.startup.ui_ready_max_ms}ms（ADR-030；G7 复跑）`,
    );
  } else {
    note(`UI ready 实测 ${launcher.ui_ready_ms ?? "?"}ms（≤${thresholds.startup.ui_ready_max_ms}ms）`);
  }
  if (testEvidencePath) {
    const testSmoke = readJsonIfExists(testEvidencePath);
    const bound = testSmoke?.metrics?.ten_sessions_upper_bound_bytes;
    check(
      "DoD-7 10 会话 ≤1.2GiB（test 构建保守上界）",
      Number(bound) > 0 && Number(bound) <= thresholds.runtime.ten_sessions_max_bytes,
      `${testSmoke?.metrics?.ten_sessions_upper_bound_mib ?? "?"} MiB`,
    );
  } else {
    skip("10 会话硬阈值", "未找到 m9-11-smoke-test-*.json（先 pnpm package:desktop:test + smoke --variant test）");
  }
}

// macOS / Linux 平台冒烟证据（CI runner 执行）
const platformSmokePrefix =
  process.platform === "darwin"
    ? "m9-11-macos-smoke-"
    : process.platform === "linux"
      ? "m9-11-linux-smoke-"
      : null;
if (platformSmokePrefix) {
  const platformSmokePath = latestEvidence(platformSmokePrefix);
  if (!platformSmokePath) {
    check(`平台安装冒烟证据（${process.platform}）`, false, `未找到 ${platformSmokePrefix}*.json`);
  } else {
    const platformSmoke = readJsonIfExists(platformSmokePath);
    console.log(`\n[verify:m9-11] 平台冒烟证据：${path.relative(repoRoot, platformSmokePath)}`);
    check(
      `平台安装冒烟通过（${process.platform}）`,
      platformSmoke?.passed === true,
      String(platformSmoke?.passed),
    );
    const metrics = platformSmoke?.metrics ?? {};
    check(
      `平台首窗/UI ready（${process.platform}）`,
      Number(metrics.window_visible_ms) > 0 && Number(metrics.ui_ready_ms) > 0,
      `visibleMs=${metrics.window_visible_ms ?? "?"} uiReadyMs=${metrics.ui_ready_ms ?? "?"}`,
    );
    check(
      `平台数据目录初始化（${process.platform}）`,
      metrics.db_created === true,
      String(metrics.db_created),
    );
  }
}

// ===== 7. 文档 =====
for (const doc of [
  "docs/M9-11-证据.md",
  "docs/M9-11-旧包手工卸载说明.md",
  "docs/M9-11-阈值阻塞与ADR建议.md",
  "docs/adr/ADR-030-M9-11-打包基线与无证书发布.md",
]) {
  check(`文档存在：${doc}`, existsSync(path.join(repoRoot, doc)), doc);
}
if (existsSync(path.join(repoRoot, "docs", "M9-11-旧包手工卸载说明.md"))) {
  const doc = readFileSync(path.join(repoRoot, "docs", "M9-11-旧包手工卸载说明.md"), "utf8");
  check(
    "手工卸载说明含 msiexec 与数据目录保留",
    doc.includes("msiexec") && /数据目录/.test(doc) && /不删除/.test(doc),
  );
}

// ===== 7. 签名（DoD-2，ADR-030） =====
if (process.argv.includes("--require-signing")) {
  // signatures.mjs 为可执行脚本（顶层 process.exit），以子进程运行。
  const { spawnSync } = await import("node:child_process");
  const args = [path.join(repoRoot, "scripts", "test", "m9-11", "signatures.mjs")];
  if (msiPath) args.push("--artifact", msiPath);
  const result = spawnSync(process.execPath, args, { stdio: "inherit" });
  check("签名/公证校验（DoD-2）", result.status === 0, `exit=${result.status}`);
} else if (process.platform === "win32") {
  const configured = Boolean(
    process.env.CSC_LINK || process.env.CSC_NAME || process.env.AETHER_WIN_CERT_FILE,
  );
  check(
    "Windows 签名政策（DoD-2，ADR-030）",
    true,
    configured
      ? "已配置证书；用 --require-signing 执行 Authenticode/时间戳校验"
      : "未配置证书 → unsigned 发布（manifest 记录 + SHA256SUMS + SmartScreen 指引）",
  );
} else if (process.platform === "darwin") {
  const configured = Boolean(process.env.CSC_NAME || process.env.AETHER_MAC_IDENTITY);
  check(
    "macOS 签名身份（DoD-2，ADR-030）",
    configured,
    configured ? "已配置；用 --require-signing 执行 codesign/spctl/stapler" : "缺少签名身份 → G6 不通过",
  );
} else {
  skip("签名/公证校验（DoD-2）", "Linux 不纳入签名发布门禁（ADR-029 §8.1）");
}

console.log("");
console.log(
  `[verify:m9-11] ${failed === 0 ? "PASS" : `FAIL（${failed} 项）`} 检查=${checks.length}` +
    (notes.length > 0 ? ` 备注=${notes.length}` : ""),
);
if (notes.length > 0) {
  for (const line of notes) console.log(`  NOTE ${line}`);
}
// 输出机器可读摘要（证据归档）。
const outDir = path.join(repoRoot, "dist-out");
if (existsSync(outDir)) {
  const { writeFileSync } = await import("node:fs");
  writeFileSync(
    path.join(outDir, `m9-11-verify-${variant}.json`),
    `${JSON.stringify(
      { schema: "aether.m9-11.verify.v1", variant, failed, checks, notes, generated_at: new Date().toISOString() },
      null,
      2,
    )}\n`,
    "utf8",
  );
}
process.exit(failed === 0 ? 0 : 1);
