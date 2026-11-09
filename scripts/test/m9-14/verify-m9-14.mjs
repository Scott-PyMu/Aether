#!/usr/bin/env node
/**
 * `pnpm verify:m9-14`（P2 计划 §2.14 / §2.15 G8；ADR-029 v0.4 D-029-11/D-029-12）。
 *
 * 校验：
 *  1. Electron 为默认构建/发布；无 Tauri 回退包/回退脚本；
 *  2. `crates/aether-tauri` 已退役（默认 workspace/构建/依赖树清零）；
 *  3. `@tauri-apps` 在 `apps/desktop/src` 与 `packages/protocol` 为 0；
 *  4. 迁移期 `bindings.ts` / `bindings.mjs` / Tauri 构建脚本已删除；
 *  5. T4 等断言已迁移到 `aether-app`（非删除）；
 *  6. CI/发布工作流只构建 Electron + `aetherd`；
 *  7. README/AGENTS/设计/P2/发布说明/G8 报告已更新；
 *  8. G4–G7 失败处理演练证据通过（停止发布 → 修复 → 重跑；无 Tauri 回退）；
 *  9. 命令合同 63/59/4、UI 61、迁移 0001–0009 不变。
 *
 * 用法：node scripts/test/m9-14/verify-m9-14.mjs
 */
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
let failed = 0;

function check(name, ok, detail = "") {
  checks.push({ name, ok, detail });
  if (ok) console.log(`PASS  ${name}${detail ? `  ${detail}` : ""}`);
  else {
    failed += 1;
    console.error(`FAIL  ${name}${detail ? `  ${detail}` : ""}`);
  }
}

function read(rel) {
  return readFileSync(path.join(repoRoot, rel), "utf8");
}

function note(name, detail = "") {
  checks.push({ name, ok: true, note: true, detail });
  console.log(`NOTE  ${name}${detail ? `  ${detail}` : ""}`);
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(repoRoot, rel));
}

function walk(relDir, extensions, out = []) {
  const absolute = path.join(repoRoot, relDir);
  if (!existsSync(absolute)) return out;
  for (const entry of readdirSync(absolute, { withFileTypes: true })) {
    if (["node_modules", "dist", "coverage", ".tmp", ".git"].includes(entry.name)) continue;
    const rel = path.posix.join(relDir, entry.name);
    if (entry.isDirectory()) walk(rel, extensions, out);
    else if (extensions.some((ext) => entry.name.endsWith(ext))) out.push(rel);
  }
  return out;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: options.timeout ?? 120_000,
    maxBuffer: 64 * 1024 * 1024,
  });
  return {
    status: result.status ?? 1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
  };
}

const pkg = readJson("package.json");
const desktopPkg = readJson("apps/desktop/package.json");
const protocolPkg = readJson("packages/protocol/package.json");

// ===== 1. Electron 默认 / 无回退包 =====
for (const [name, command] of [
  ["verify:m9-14", "scripts/test/m9-14/verify-m9-14.mjs"],
  ["m9-14:failure-drill", "scripts/test/m9-14/failure-drill.mjs"],
  ["dev:desktop", "scripts/ci/dev-desktop.mjs"],
  ["package:desktop", "scripts/ci/package-desktop.mjs"],
  ["smoke:installer", "scripts/test/m9-11/smoke-installer.mjs"],
]) {
  check(`package.json scripts.${name}`, pkg.scripts?.[name]?.includes(command) === true, pkg.scripts?.[name] ?? "缺失");
}
check(
  "package.json 无 Tauri 构建/回退脚本",
  !Object.entries(pkg.scripts ?? {}).some(
    ([name, command]) => /tauri/i.test(name) || /\btauri\b/i.test(command),
  ),
  Object.keys(pkg.scripts ?? {}).filter((name) => /tauri/i.test(name)).join(",") || "0 命中",
);
check(
  "apps/desktop 无 @tauri-apps 依赖/脚本",
  !Object.keys(desktopPkg.dependencies ?? {}).some((name) => name.startsWith("@tauri-apps")) &&
    !Object.keys(desktopPkg.devDependencies ?? {}).some((name) => name.startsWith("@tauri-apps")) &&
    !Object.prototype.hasOwnProperty.call(desktopPkg.scripts ?? {}, "tauri"),
);
check(
  "packages/protocol 无 @tauri-apps 依赖/生成步骤",
  !Object.keys(protocolPkg.dependencies ?? {}).some((name) => name.startsWith("@tauri-apps")) &&
    !Object.keys(protocolPkg.devDependencies ?? {}).some((name) => name.startsWith("@tauri-apps")) &&
    !Object.values(protocolPkg.scripts ?? {}).some((command) => /bindings\.mjs|@tauri-apps/.test(command)),
);

// ===== 2. Tauri crate 退役 =====
check("crates/aether-tauri 已删除", !exists("crates/aether-tauri"));
check("迁移期 bindings.ts 已删除", !exists("packages/protocol/src/bindings.ts"));
check("迁移期 bindings.mjs 已删除", !exists("scripts/ci/bindings.mjs"));
check("Tauri 构建脚本已删除", !exists("scripts/ci/build-desktop-installer.mjs"));
check("Tauri 壳冒烟脚本已删除", !exists("scripts/test/smoke-desktop.mjs"));
check("cargo-dist Tauri 发布工作流已删除", !exists(".github/workflows/release.yml"));
check("旧 Tauri 夜跑工作流已删除", !exists(".github/workflows/m4-nightly.yml") && !exists(".github/workflows/m8-nightly.yml"));

const workspace = read("Cargo.toml");
const members = [...workspace.matchAll(/^\s*"(crates\/aether-[a-z-]+)",?$/gm)].map((match) => match[1]);
const expectedMembers = [
  "crates/aether-core",
  "crates/aether-store",
  "crates/aether-adapters",
  "crates/aether-control",
  "crates/aether-security",
  "crates/aether-rpc",
  "crates/aether-app",
  "crates/aether-daemon",
];
check(
  "Cargo workspace members = 8（无 aether-tauri）",
  JSON.stringify(members.sort()) === JSON.stringify(expectedMembers.sort()) &&
    !/^aether-tauri\s*=/m.test(workspace),
  members.join(","),
);

const cargoLock = read("Cargo.lock");
check(
  "Cargo.lock 无 aether-tauri/tauri 依赖",
  !/^name = "aether-tauri"$/m.test(cargoLock) && !/^name = "tauri(-[a-z0-9-]+)?"$/m.test(cargoLock),
  "0 命中",
);
const pnpmLock = read("pnpm-lock.yaml");
check("pnpm-lock.yaml 无 @tauri-apps", !pnpmLock.includes("@tauri-apps"), "0 命中");

const metadata = run("cargo", ["metadata", "--no-deps", "--format-version", "1"], { timeout: 300_000 });
if (metadata.status !== 0) {
  check("cargo metadata", false, metadata.stderr.slice(-400));
} else {
  const parsed = JSON.parse(metadata.stdout);
  const names = (parsed.packages ?? []).map((entry) => entry.name);
  check("cargo metadata 无 aether-tauri", !names.includes("aether-tauri"), names.join(","));
  check(
    "cargo metadata 默认成员无 aether-tauri",
    !(parsed.workspace_default_members ?? []).some((id) => String(id).includes("aether-tauri")),
    `${(parsed.workspace_default_members ?? []).length} 个默认成员`,
  );
}
const tree = run("cargo", ["tree", "-p", "aether-app", "-p", "aether-daemon"], { timeout: 300_000 });
check(
  "cargo tree -p aether-app -p aether-daemon 无 Tauri",
  tree.status === 0 && !/tauri|wry|webkit2gtk/i.test(tree.stdout),
  tree.status === 0 ? "0 命中" : tree.stderr.slice(-400),
);

// ===== 3. @tauri-apps 清零 =====
const tauriText = [];
for (const rel of [
  ...walk("apps/desktop/src", [".ts", ".tsx", ".js", ".mjs", ".cjs", ".json", ".html", ".css"]),
  ...walk("packages/protocol", [".ts", ".tsx", ".js", ".mjs", ".cjs", ".json", ".md"]),
]) {
  if (read(rel).includes("@tauri-apps")) tauriText.push(rel);
}
check(
  "apps/desktop/src + packages/protocol 的 @tauri-apps = 0",
  tauriText.length === 0,
  tauriText.join(",") || "0 命中",
);

// ===== 4. 断言迁移（T4/预算/审计）=====
for (const rel of [
  "crates/aether-app/tests/m3_06_t4.rs",
  "crates/aether-app/tests/m3_02_session_backend.rs",
  "crates/aether-app/tests/m6_03_audit.rs",
]) {
  check(`迁移后的测试存在：${rel}`, exists(rel));
}
const regression = read("scripts/test/m9-13/regression.mjs");
const mapping = readJson("scripts/test/m9-13/p0-p1-mapping.json");
check(
  "M9-13 T4 已指向 aether-app（不再构建 Tauri）",
  regression.includes('"aether-app"') &&
    regression.includes('"m3_06_t4"') &&
    mapping.p0?.find((entry) => entry.id === "T4")?.command?.includes("aether-app") === true,
);

// ===== 5. CI/发布工作流 =====
const workflowDir = path.join(repoRoot, ".github", "workflows");
const workflows = readdirSync(workflowDir).filter((name) => name.endsWith(".yml"));
const forbiddenWorkflow = [];
for (const name of workflows) {
  const text = read(`.github/workflows/${name}`);
  for (const token of ["aether-tauri", "build-desktop-installer", "smoke-desktop", "@tauri-apps", "cargo-dist", "tauri build"]) {
    if (text.includes(token)) forbiddenWorkflow.push(`${name}:${token}`);
  }
}
check(
  "CI/发布工作流无 Tauri 构建命令",
  forbiddenWorkflow.length === 0,
  forbiddenWorkflow.join(",") || "0 命中",
);
const releaseWorkflow = read(".github/workflows/release-desktop-installers.yml");
check(
  "发布工作流使用 electron-builder（package:desktop）",
  releaseWorkflow.includes("pnpm package:desktop") && releaseWorkflow.includes("gh release"),
  "msi/dmg/deb/AppImage",
);
check(
  "macOS 公开发布仍失败关闭（人工授权未就绪不发布 DMG）",
  releaseWorkflow.includes("M9_11_MAC_SIGNING_READY") &&
    releaseWorkflow.includes("macOS 签名/公证未就绪") &&
    releaseWorkflow.includes("exit 1"),
  "release-desktop-installers.yml",
);
check(
  "Electron 打包图标不再依赖已退役 crate",
  read("apps/desktop/electron-builder.config.mjs").includes('"assets", "icons"') &&
    !read("apps/desktop/electron-builder.config.mjs").includes("aether-tauri"),
);
for (const icon of ["icon.ico", "icon.icns", "icon.png"]) {
  check(`Electron 图标存在：${icon}`, exists(`apps/desktop/assets/icons/${icon}`));
}

// ===== 6. 文档/发布说明/G8 报告 =====
const requiredDocs = {
  "README.md": ["已退役", "aether-tauri", "pnpm dev:desktop", "pnpm package:desktop", "M10-00 已执行"],
  "docs/P2-架构迁移-实施计划与验收标准.md": [
    "v0.11",
    "M9-14",
    "docs/M9-14-证据.md",
    "ADR-030 v0.2",
    "M10-00 已执行",
  ],
  "docs/M9-14-证据.md": ["DoD-1", "DoD-2", "DoD-3", "DoD-4", "DoD-5"],
  "docs/M9-14-发布说明.md": ["旧内测包", "手工卸载", "数据目录", "Keychain"],
  "docs/M9-14-macOS-签名授权方案.md": [
    "人工授权",
    "Developer ID",
    "不阻塞 M9/M10",
    "公开发布",
  ],
  "docs/M9-Gate-G8-报告.md": ["G8", "签核", "conditional_pass", "M10 可启动"],
  "docs/adr/ADR-030-M9-11-打包基线与无证书发布.md": ["v0.2", "M9/M10"],
};
// AGENTS.md / 设计文档.md 在本仓库为本地受控文档（.gitignore 排除，不入 CI checkout）；
// 本机存在时强制校验，CI 缺失时显式 NOTE（不静默跳过）。
const localDocs = {
  "AGENTS.md": ["已退役", "v1.33", "v0.11", "M10-00/决策记录"],
  "设计文档.md": ["v1.33", "已退役", "不阻塞 M9/M10"],
};
for (const [rel, tokens] of Object.entries(requiredDocs)) {
  if (!exists(rel)) {
    check(`文档存在：${rel}`, false);
    continue;
  }
  const text = read(rel);
  const missing = tokens.filter((token) => !text.includes(token));
  check(`文档 ${rel} 含 G8 标记`, missing.length === 0, missing.join(",") || "全部命中");
}
for (const [rel, tokens] of Object.entries(localDocs)) {
  if (!exists(rel)) {
    note(`本地文档 ${rel} 不在 CI checkout（gitignored）`, "跳过；发布评审需在本地复核");
    continue;
  }
  const text = read(rel);
  const missing = tokens.filter((token) => !text.includes(token));
  check(`本地文档 ${rel} 含 G8 标记`, missing.length === 0, missing.join(",") || "全部命中");
}

// ===== 7. 失败处理演练 =====
const drill = run(process.execPath, ["scripts/test/m9-14/failure-drill.mjs"], { timeout: 120_000 });
check("G4–G7 失败处理演练执行", drill.status === 0, drill.status === 0 ? "exit=0" : drill.stderr.slice(-400));
const drillPath = "scripts/test/.tmp/m9-14/failure-drill.json";
if (!exists(drillPath)) {
  check("失败处理演练证据存在", false, drillPath);
} else {
  const report = readJson(drillPath);
  const gates = new Set((report.gates ?? []).map((entry) => entry.id));
  check("失败处理演练 passed", report.passed === true, String(report.passed));
  check(
    "失败处理演练覆盖 G4/G5/G6/G7",
    ["G4", "G5", "G6", "G7"].every((id) => gates.has(id)),
    [...gates].join(","),
  );
  check(
    "失败处理策略：停止发布/修复重跑/无 Tauri 回退",
    report.policy?.gate_failure === "stop_release" &&
      report.policy?.fix === "fix_then_rerun_gate" &&
      report.policy?.tauri_fallback === "forbidden",
    JSON.stringify(report.policy ?? {}),
  );
  check(
    "演练步骤全部通过",
    (report.steps ?? []).every((step) => step.ok === true) && (report.steps ?? []).length >= 12,
    `${(report.steps ?? []).length} 步`,
  );
}

// ===== 8. 命令/迁移冻结面 =====
const manifest = readJson("packages/protocol/src/command-manifest.json");
const commands = manifest.commands ?? manifest;
const total = Array.isArray(commands) ? commands.length : commands.total;
const daemon = Array.isArray(commands)
  ? commands.filter((item) => item.route === "daemon").length
  : commands.daemon;
const shell = Array.isArray(commands)
  ? commands.filter((item) => item.route === "shell").length
  : commands.shell;
const ui = Array.isArray(commands)
  ? commands.filter((item) => item.exposure === "ui").length
  : commands.ui;
check(
  "命令合同 ≥ M9 基线 63/59/4/UI 61",
  total >= 63 && daemon >= 59 && shell === 4 && ui >= 61,
  `${total}/${daemon}/${shell}/${ui}`,
);
check("daemon 59 / shell 4", daemon >= 59 && shell === 4, `${daemon}/${shell}`);
const migrations = readdirSync(path.join(repoRoot, "migrations"))
  .filter((name) => /^\d{4}_.*\.sql$/.test(name))
  .sort();
const frozenMigrations = [
  "0001_init.sql",
  "0002_unique_keys.sql",
  "0003_p0_ui_extensions.sql",
  "0004_permission_mode_and_rules.sql",
  "0005_audit_indexes.sql",
  "0006_runtime_enabled.sql",
  "0007_presets.sql",
  "0008_session_search_fts.sql",
  "0009_provider_endpoints.sql",
];
check(
  "迁移 0001–0009 未改名，仅允许 0010+ 追加",
  frozenMigrations.every((name, index) => migrations[index] === name) &&
    migrations.slice(frozenMigrations.length).every((name) => name > "0009"),
  migrations.join(","),
);

console.log("");
console.log(`[verify:m9-14] ${failed === 0 ? "PASS" : `FAIL（${failed} 项）`} 检查=${checks.length}`);
if (failed > 0) process.exit(1);
console.log("AETHER_M9_14 PASS");
process.exit(0);
