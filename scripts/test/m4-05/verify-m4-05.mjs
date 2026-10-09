/**
 * M4-05 验证入口：打包与发布（实施计划 v1.24 §5 M4-05）。
 *
 * 覆盖 DoD：
 *   1) Tauri 产出 MSI/DMG（cargo-dist 编排上传）+ 安装产物内置官方运行时注册
 *      （三官方集合，ADR-008；Mock 仅测试构建）+ 真实 WebView 内联权限回环 E2E；
 *   2) WebView2 引导安装配置（embedBootstrapper）；
 *   3) 签名步骤挂接流水线（MVP 空签可执行）+ 产物哈希记录。
 *
 * 组成：
 *   - 静态检查：`tauri.conf.json`（targets/resources/webviewInstallMode）、
 *     `build-desktop-installer.mjs`（哈希清单）、`release-desktop-installers.yml`
 *     （签名挂接 + 产物上传 + 哈希）、ADR-016 能力登记；
 *   - 发布形态注册包（三官方、无 Mock）构建 + 清单 schema + 摘要校验；
 *   - 真实 WebView 内联权限回环 E2E（`e2e-inline-permission-loop.mjs`）；
 *   - 安装产物冒烟（`installer-smoke.mjs`；`--with-installer` 时执行，夜跑包含）。
 *
 * 用法：node scripts/test/m4-05/verify-m4-05.mjs [--with-installer]
 *   [--skip-frontend-build] [--skip-rust-build] [--skip-bundle-build]
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { repoRoot, run, summarize } from "../lib/exec.mjs";
import { stampNow } from "../m4/lib/drill.mjs";

const checks = [];
const record = (name, ok, detail) =>
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });

const withInstaller = process.argv.includes("--with-installer");
const passthrough = process.argv
  .slice(2)
  .filter((arg) => arg.startsWith("--skip-"));
const evidenceDir = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m4-05",
  `evidence-${stampNow()}`,
);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. 静态检查（打包配置 / 哈希 / 签名挂接 / ADR-016）=====

{
  const problems = [];
  const tauriConf = JSON.parse(
    readFileSync(path.join(repoRoot, "crates", "aether-tauri", "tauri.conf.json"), "utf8"),
  );
  const targets = tauriConf.bundle?.targets ?? [];
  if (!targets.includes("msi") || !targets.includes("dmg")) {
    problems.push(`bundle.targets 必须含 msi/dmg：${JSON.stringify(targets)}`);
  }
  const resources = JSON.stringify(tauriConf.bundle?.resources ?? []);
  if (!resources.includes("runtime-bundle")) {
    problems.push("bundle.resources 必须包含 runtime-bundle（安装产物内注册）");
  }
  const webview = tauriConf.bundle?.windows?.webviewInstallMode?.type;
  if (webview !== "embedBootstrapper") {
    problems.push(`webviewInstallMode 必须为 embedBootstrapper：${webview}`);
  }

  const installerScript = readFileSync(
    path.join(repoRoot, "scripts", "ci", "build-desktop-installer.mjs"),
    "utf8",
  );
  for (const [needle, label] of [
    ["SHA256SUMS.txt", "产物哈希清单"],
    ["build-runtime-bundles.mjs", "运行时注册包构建挂接"],
    ["--bundles", "Tauri 安装器构建"],
  ]) {
    if (!installerScript.includes(needle)) problems.push(`build-desktop-installer.mjs 缺少${label}`);
  }

  const releaseWorkflow = readFileSync(
    path.join(repoRoot, ".github", "workflows", "release-desktop-installers.yml"),
    "utf8",
  );
  for (const [needle, label] of [
    ["signtool", "Windows 签名步骤（MVP 空签可执行）"],
    ["notarytool", "macOS 公证步骤（MVP 空签可执行）"],
    ["SHA256SUMS.txt", "哈希清单上传"],
  ]) {
    if (!releaseWorkflow.includes(needle)) problems.push(`release-desktop-installers.yml 缺少${label}`);
  }

  const capability = JSON.parse(
    readFileSync(
      path.join(repoRoot, "crates", "aether-tauri", "capabilities", "default.json"),
      "utf8",
    ),
  );
  const allowed = ["core:event:allow-listen", "core:event:allow-unlisten"];
  const permissions = capability.permissions ?? [];
  if (
    permissions.length !== allowed.length ||
    !allowed.every((entry) => permissions.includes(entry))
  ) {
    problems.push(`capabilities 权限集必须为 ADR-016 最小监听对：${JSON.stringify(permissions)}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：打包配置/哈希/签名挂接/ADR-016 能力登记",
    problems.length === 0,
    problems.join("; "),
  );
}

// ===== 2. 发布形态注册包（三官方、无 Mock）=====

{
  const releaseBundle = path.join(evidenceDir, "release-runtime-bundle");
  const build = spawnSync(
    process.execPath,
    [
      path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs"),
      "--out",
      releaseBundle,
    ],
    { cwd: repoRoot, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 },
  );
  process.stdout.write(`${build.stdout ?? ""}${build.stderr ?? ""}`);
  const manifestPath = path.join(releaseBundle, "runtimes.json");
  const manifest = existsSync(manifestPath)
    ? JSON.parse(readFileSync(manifestPath, "utf8"))
    : null;
  const digestsPath = path.join(releaseBundle, "digests.json");
  const digests = existsSync(digestsPath) ? JSON.parse(readFileSync(digestsPath, "utf8")) : null;
  const ids = manifest?.runtimes?.map((entry) => entry.id) ?? [];
  const expectedIds = ["claude-code", "codex", "deepseek-harness"];
  const officialOk =
    manifest?.schema_version === 1 &&
    ids.length === 3 &&
    expectedIds.every((id) => ids.includes(id)) &&
    !ids.includes("mock");
  let digestOk = Boolean(digests);
  if (digestOk) {
    for (const entry of manifest.runtimes) {
      const programPath = path.join(releaseBundle, entry.program);
      if (!existsSync(programPath)) {
        digestOk = false;
        break;
      }
      const actual = `sha256:${createHash("sha256").update(readFileSync(programPath)).digest("hex")}`;
      if (digests.digests?.[entry.program] !== actual) {
        digestOk = false;
        break;
      }
    }
  }
  writeFileSync(
    path.join(evidenceDir, "release-bundle.json"),
    `${JSON.stringify({ manifest, digests_ok: digestOk, build_exit: build.status }, null, 2)}\n`,
    "utf8",
  );
  record(
    "发布形态注册包：三官方集合（无 Mock）+ ADR-015 schema + 摘要一致",
    build.status === 0 && officialOk && digestOk,
    `ids=${ids.join(",")}`,
  );
}

// ===== 3. 真实 WebView 内联权限回环 E2E =====

record(
  "真实 WebView 内联权限回环 E2E（含事件通道监听/解除监听注册；ADR-016）",
  run(process.execPath, [
    path.join(repoRoot, "scripts", "test", "m4-05", "e2e-inline-permission-loop.mjs"),
    ...passthrough,
  ]) === 0,
);

// ===== 4. 安装产物冒烟（可选；夜跑包含）=====

if (withInstaller) {
  record(
    "安装产物冒烟：MSI 哈希 → 解包 → 首启自检 → 注册清单 → 会话闭环",
    run(process.execPath, [path.join(repoRoot, "scripts", "test", "m4-05", "installer-smoke.mjs")]) ===
      0,
  );
} else {
  record("安装产物冒烟（--with-installer 时执行；本机含 MSI 构建）", true, "skipped");
}

// ===== 5. 证据归档 =====

writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(
    {
      task: "M4-05",
      stamp: path.basename(evidenceDir),
      with_installer: withInstaller,
      platform: `${os.platform()}-${os.arch()}`,
      checks,
    },
    null,
    2,
  )}\n`,
  "utf8",
);
console.log(`[m4-05] 证据目录：${evidenceDir}`);
process.exit(summarize("verify-m4-05", checks));
