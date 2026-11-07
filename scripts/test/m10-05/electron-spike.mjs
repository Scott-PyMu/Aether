#!/usr/bin/env node
/**
 * Q3 打包 spike（M10-05 前置；ADR-035 v1.1 / 决策记录 §9-4）。
 *
 * 用一个最小 Electron 应用验证：
 * - `@napi-rs/image` N-API 在 asarUnpack 后可由 UtilityProcess 加载；
 * - 四格式解码与 25MP 文件头拒绝；
 * - Windows MSI 可构建（macOS/Linux 需对应平台 runner，见输出 pending）。
 *
 * 运行：node scripts/test/m10-05/electron-spike.mjs
 * 依赖：根 `electron-builder`、`apps/desktop/node_modules/electron/dist`、npm 网络（首次）。
 */
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const sourceDir = path.join(repoRoot, "scripts", "test", "m10-05", "electron-spike");
const workDir = path.join(repoRoot, "scripts", "test", "m10-05", ".tmp", "electron-spike");
const appDir = path.join(workDir, "app");
const outDir = path.join(workDir, "dist");
const resultsPath = path.join(repoRoot, "scripts", "test", "m10-05", ".tmp", "q3-electron-spike-results.json");
const skipInstall = process.env.AETHER_SPIKE_SKIP_INSTALL === "1";
const skipBuild = process.env.AETHER_SPIKE_SKIP_BUILD === "1";
const results = { schema: "aether.m10-05.q3-electron-spike.v1", started_at: new Date().toISOString(), checks: [], pending: [] };

function check(name, ok, detail) {
  results.checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
  if (!ok) throw new Error(`Q3 Electron check failed: ${name}`);
}

function run(command, args, options = {}) {
  const isCmd = process.platform === "win32" && (command === "npm" || command.endsWith(".cmd") || command.endsWith(".bat"));
  const actualCommand = isCmd ? process.env.ComSpec ?? "cmd.exe" : command;
  const actualArgs = isCmd ? ["/d", "/s", "/c", command, ...args] : args;
  console.log(`[spike] ${actualCommand} ${actualArgs.join(" ")}`);
  const result = spawnSync(actualCommand, actualArgs, {
    cwd: options.cwd ?? repoRoot,
    encoding: "utf8",
    timeout: options.timeout ?? 600_000,
    env: { ...process.env, ...(options.env ?? {}) },
    stdio: options.stdio ?? "pipe",
  });
  if (result.status !== 0) {
    console.error(result.stdout ?? "");
    console.error(result.stderr ?? "");
    throw new Error(`${command} exited ${result.status}`);
  }
  return result;
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

// 1. 准备最小应用目录。
if (!skipInstall && !skipBuild) {
  rmSync(workDir, { recursive: true, force: true });
}
mkdirSync(appDir, { recursive: true });
for (const file of ["package.json", "main.cjs", "worker.cjs", "node-check.cjs"]) {
  cpSync(path.join(sourceDir, file), path.join(appDir, file));
}
if (!skipInstall) {
  run("npm", ["install", "--omit=dev", "--no-package-lock", "--no-audit", "--no-fund"], { cwd: appDir, timeout: 600_000 });
} else {
  check("复用已安装 node_modules", existsSync(path.join(appDir, "node_modules", "@napi-rs", "image")), appDir);
}

const electronDist = path.join(repoRoot, "apps", "desktop", "node_modules", "electron", "dist");
check("Electron 本机发行目录存在", existsSync(path.join(electronDist, "electron.exe")), electronDist);
const electronVersion = JSON.parse(readFileSync(path.join(repoRoot, "apps", "desktop", "node_modules", "electron", "package.json"), "utf8")).version;

const configPath = path.join(workDir, "electron-builder.config.cjs");
writeFileSync(
  configPath,
  `module.exports = {
  appId: "dev.aether.m10-05.spike",
  productName: "AetherDecodeSpike",
  copyright: "Copyright (c) 2026 Aether Contributors (Q3 spike only)",
  directories: { app: ${JSON.stringify(appDir)}, output: ${JSON.stringify(outDir)} },
  asar: true,
  compression: "store",
  electronVersion: ${JSON.stringify(electronVersion)},
  electronDist: ${JSON.stringify(electronDist)},
  npmRebuild: false,
  removePackageScripts: true,
  files: ["main.cjs", "worker.cjs", "node-check.cjs", "package.json", "node_modules/**/*"],
  asarUnpack: ["**/node_modules/@napi-rs/**", "worker.cjs", "node-check.cjs"],
  win: { target: ["msi"], icon: ${JSON.stringify(path.join(repoRoot, "apps", "desktop", "assets", "icons", "icon.ico"))}, signAndEditExecutable: true, forceCodeSigning: false },
  msi: { oneClick: true, perMachine: false, runAfterFinish: false },
};
`,
  "utf8",
);

// 2. 构建 Windows MSI（同时产出 win-unpacked）。
if (!skipBuild) {
  const cli = path.join(repoRoot, "node_modules", "electron-builder", "out", "cli", "cli.js");
  check("electron-builder CLI 存在", existsSync(cli), cli);
  mkdirSync(outDir, { recursive: true });
  run(process.execPath, [cli, "--config", configPath, "--publish", "never", "--win", "msi", "--x64"], {
    cwd: workDir,
    timeout: 900_000,
  });
} else {
  check("复用既有打包产物", existsSync(outDir), outDir);
}

const unpackedDir = path.join(outDir, "win-unpacked");
const exeCandidates = readdirSync(unpackedDir).filter((name) => name.endsWith(".exe"));
const exe = path.join(unpackedDir, exeCandidates.find((name) => name.startsWith("AetherDecodeSpike")) ?? exeCandidates[0]);
check("win-unpacked 应用可执行文件", existsSync(exe), path.relative(repoRoot, exe));
const msies = readdirSync(outDir).filter((name) => name.endsWith(".msi"));
check("Windows MSI 产物", msies.length === 1, msies.join(","));
const msi = path.join(outDir, msies[0]);
results.windows_msi = { name: msies[0], bytes: statSync(msi).size, sha256: sha256(msi) };

// 3. asarUnpack 断言：native .node 必须在 unpacked，且 asar 内不得出现 .node。
const resources = path.join(unpackedDir, "resources");
const asarPath = path.join(resources, "app.asar");
const unpackedNodeModules = path.join(resources, "app.asar.unpacked", "node_modules");
check("app.asar 存在", existsSync(asarPath), path.relative(repoRoot, asarPath));
const nativeDir = path.join(unpackedNodeModules, "@napi-rs", "image-win32-x64-msvc");
const nativeNode = path.join(nativeDir, "image.win32-x64-msvc.node");
check("N-API .node 已解包", existsSync(nativeNode), path.relative(repoRoot, nativeNode));
check("@napi-rs/image JS 已解包", existsSync(path.join(unpackedNodeModules, "@napi-rs", "image", "index.js")), path.relative(repoRoot, unpackedNodeModules));
check("worker.cjs 已解包", existsSync(path.join(resources, "app.asar.unpacked", "worker.cjs")), "app.asar.unpacked/worker.cjs");

const { readAsarHeader } = await import("../../../scripts/ci/asar.mjs");
const asarHeader = readAsarHeader(asarPath);
const asarEntries = [];
const walkAsar = (node, prefix) => {
  for (const [name, value] of Object.entries(node?.files ?? {})) {
    const full = `${prefix}/${name}`;
    if (value && typeof value === "object" && value.files) walkAsar(value, full);
    else if (value && typeof value === "object") asarEntries.push({ path: full, unpacked: value.unpacked === true, size: value.size });
  }
};
walkAsar(asarHeader, "");
const nodeEntries = asarEntries.filter((entry) => entry.path.endsWith(".node"));
check(
  "asar 内 .node 全部标记 unpacked（内容在 app.asar.unpacked）",
  nodeEntries.length > 0 && nodeEntries.every((entry) => entry.unpacked),
  nodeEntries.map((entry) => `${entry.path}=${entry.unpacked}`).join(","),
);

// 4. 运行打包后的应用，由 UtilityProcess 执行解码。
// 本机 agent 会话为非交互式桌面，Electron GUI 初始化会 0xC0000005；脚本不隐藏该阻断，
// 而是记录 blocked 并用 ELECTRON_RUN_AS_NODE 验证 N-API/asarUnpack 的 Electron ABI。
const spikeOut = path.join(workDir, "spike-result.json");
const userDataDir = path.join(workDir, "userdata");
rmSync(userDataDir, { recursive: true, force: true });
let spike = null;
let utilityError = null;
try {
  run(exe, ["--no-sandbox", "--disable-gpu", `--user-data-dir=${userDataDir}`, "--spike", `--spike-out=${spikeOut}`], { cwd: workDir, timeout: 120_000 });
  if (!existsSync(spikeOut)) throw new Error("spike 结果文件未生成");
  spike = JSON.parse(readFileSync(spikeOut, "utf8"));
} catch (error) {
  utilityError = error instanceof Error ? error.message : String(error);
  const environmentCrash = /2147483651|3221225477|-1073741819|-1073741821|0xC0000005|0x80000003/i.test(utilityError);
  if (!environmentCrash) throw error;
  results.utility_process = {
    status: "blocked",
    reason: "Electron GUI 初始化在非交互式 agent 会话崩溃（0xC0000005）；需交互式 Windows 桌面/CI runner 复跑",
    error: utilityError,
  };
  console.error(`[spike] UtilityProcess 运行时 blocked：${utilityError}`);
}
if (spike !== null) {
  check("UtilityProcess 返回 ok", spike.ok === true, JSON.stringify(spike.error ?? null));
  check("打包态 app.isPackaged", spike.worker?.is_packaged === true);
  check("appRoot 位于 app.asar", spike.worker?.app_root_is_asar === true, spike.worker?.app_root ?? "");
  check(
    "UtilityProcess 解析 native 包",
    spike.worker?.native_node_exists === true && String(spike.worker?.native_package ?? "").startsWith("image-"),
    JSON.stringify({ package: spike.worker?.native_package, path: spike.worker?.native_node_path }),
  );
  check("worker 的 .node 实际存在", spike.worker?.native_node_exists === true, spike.worker?.native_node_path ?? "");
  check("UtilityProcess 四格式解码", spike.worker?.checks?.filter((c) => c.name.endsWith("→ WebP")).every((c) => c.ok) === true, JSON.stringify(spike.worker?.checks ?? []));
  check("UtilityProcess 25MP 边界", spike.worker?.checks?.filter((c) => c.name.includes("25MP") || c.name.includes("8193")).every((c) => c.ok) === true);
  check("UtilityProcess 整体 ok", spike.worker?.ok === true);
  results.utility_process = { status: "pass", worker: spike.worker };
}

// 4b. Electron Node ABI + asarUnpack 加载（不依赖 GUI）。
const unpackedModules = path.join(resources, "app.asar.unpacked", "node_modules");
const nodeCheckPath = path.join(resources, "app.asar.unpacked", "node-check.cjs");
const nodeCheck = run(exe, [nodeCheckPath], {
  cwd: workDir,
  timeout: 60_000,
  env: {
    ELECTRON_RUN_AS_NODE: "1",
    AETHER_SPIKE_UNPACKED_MODULES: unpackedModules,
    CHROME_CRASHPAD_PIPE_NAME: "",
  },
});
const nodeCheckResult = JSON.parse(String(nodeCheck.stdout).trim().split(/\r?\n/).pop());
check("Electron Node ABI 加载 N-API", nodeCheckResult.ok === true, JSON.stringify(nodeCheckResult));
check("Electron Node 解析 native 到 asar.unpacked", String(nodeCheckResult.native_entry).includes("app.asar.unpacked"), nodeCheckResult.native_entry);
results.electron_node = nodeCheckResult;
results.electron = { version: electronVersion, msi: results.windows_msi };

// 5. 其他平台（本机 win32 无法执行；保留待 runner）。
results.pending.push("macOS 未签名 DMG：需 macOS runner 执行同一 spike 应用（asarUnpack/UtilityProcess/四格式/25MP）");
results.pending.push("Linux deb/AppImage：需 Linux runner 执行同一 spike 应用；本机不可运行");
results.finished_at = new Date().toISOString();
results.checks_ok = results.checks.every((item) => item.ok);
results.status = results.utility_process?.status === "pass" ? "pass" : "partial";
results.ok = results.status === "pass";
writeFileSync(resultsPath, `${JSON.stringify(results, null, 2)}\n`, "utf8");
console.log(`Q3 Electron spike: ${results.checks.filter((c) => c.ok).length}/${results.checks.length} checks passed; utilityProcess=${results.utility_process?.status}`);
process.exitCode = results.status === "pass" ? 0 : 2;
