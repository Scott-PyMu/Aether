#!/usr/bin/env node
/**
 * M9-11 macOS DMG 冒烟（DoD-1/DoD-4 的 macOS 部分，CI runner 执行）。
 *
 * 挂载 DMG → 复制 `.app` 到临时目录 → 校验布局 → 启动 → shell.log 首窗/UI ready →
 * 退出 → 卸载镜像。签名/公证由 `pnpm verify:signatures` 单独断言（DoD-2）。
 *
 * 用法：node scripts/test/m9-11/smoke-macos.mjs [--dmg <path>]
 */
import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { createChecks, findShellEvent, readJsonLines, sleep, waitFor } from "./lib/util.mjs";
import { summarize } from "../lib/exec.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const outDir = path.join(repoRoot, "dist-out");
const { checks, record } = createChecks();
const evidence = {
  schema: "aether.m9-11.macos-smoke.v1",
  platform: process.platform,
  arch: process.arch,
  started_at: new Date().toISOString(),
  checks,
};

function argValue(flag) {
  const index = process.argv.indexOf(flag);
  return index >= 0 ? path.resolve(process.argv[index + 1]) : null;
}

function findDmg() {
  if (!existsSync(outDir)) return null;
  const files = readdirSync(outDir)
    .filter((name) => name.startsWith("Aether-") && name.endsWith(".dmg"))
    .sort();
  return files.length === 0 ? null : path.join(outDir, files[files.length - 1]);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", timeout: 120_000, ...options });
  if (result.error) throw new Error(`${command} 无法执行：${result.error.message}`);
  return result;
}

async function launch(appBinary, root) {
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user-data");
  const home = path.join(root, "home");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  mkdirSync(path.join(home, "Downloads"), { recursive: true });
  const child = spawn(appBinary, [], {
    cwd: path.dirname(appBinary),
    env: {
      ...process.env,
      HOME: home,
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
      AETHER_USER_DATA_DIR: userData,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (chunk) => {
    stderr = `${stderr}${chunk.toString("utf8")}`.slice(-16_384);
  });
  const shellLog = path.join(dataDir, "logs", "shell.log");
  const waitEvent = (event, timeoutMs) =>
    waitFor(
      () => findShellEvent(readJsonLines(shellLog), event),
      timeoutMs,
      `shell.log ${event}`,
    );
  const result = { child, dataDir, shellLog, stderr: () => stderr, exited: null };
  child.on("exit", (code, signal) => {
    result.exited = { code, signal };
  });
  try {
    result.window = await waitEvent("window.ready", 45_000);
    result.ui = await waitEvent("ui.ready", 60_000);
    result.db = existsSync(path.join(dataDir, "aether.db"));
  } finally {
    await sleep(1_500);
    if (child.exitCode === null) {
      child.kill("SIGTERM");
      await waitFor(() => result.exited !== null, 10_000, "macOS 应用退出").catch(() => {});
    }
    if (child.exitCode === null) child.kill("SIGKILL");
  }
  return result;
}

async function main() {
  if (process.platform !== "darwin") {
    console.error("[m9-11-macos] 仅 macOS 运行（CI macos runner）。");
    process.exit(2);
  }
  const dmg = argValue("--dmg") ?? findDmg();
  record("找到 macOS DMG 产物", Boolean(dmg), dmg ?? "未找到");
  if (!dmg) return;
  evidence.artifact = {
    path: path.relative(repoRoot, dmg),
    bytes: statSync(dmg).size,
    sha256: undefined,
  };

  const work = mkdtempSync(path.join(outDir, "m9-11-macos-"));
  const mount = path.join(work, "mount");
  mkdirSync(mount, { recursive: true });
  const attach = run("hdiutil", [
    "attach",
    "-nobrowse",
    "-readonly",
    "-mountpoint",
    mount,
    dmg,
  ]);
  if (attach.status !== 0) throw new Error(`hdiutil attach 失败：${attach.stderr}`);
  try {
    const apps = readdirSync(mount).filter((name) => name.endsWith(".app"));
    record("DMG 内恰有 1 个 .app", apps.length === 1, apps.join(","));
    if (apps.length !== 1) return;
    const app = path.join(mount, apps[0]);
    const resources = path.join(app, "Contents", "Resources");
    const daemon = path.join(resources, "bin", "aetherd");
    const manifestPath = path.join(resources, "runtime-bundle", "runtimes.json");
    record("extraResources: Contents/Resources/bin/aetherd", existsSync(daemon), daemon);
    record("extraResources: runtime-bundle", existsSync(manifestPath), manifestPath);
    if (existsSync(manifestPath)) {
      const ids = (JSON.parse(readFileSync(manifestPath, "utf8")).runtimes ?? []).map(
        (entry) => entry.id,
      );
      record(
        "runtime-bundle 含三官方运行时",
        ["claude-code", "codex", "deepseek-harness"].every((id) => ids.includes(id)),
        ids.join(","),
      );
    }
    record("Contents/Resources/app.asar 存在", existsSync(path.join(resources, "app.asar")));
    record(
      "壳级页面已解包（app.asar.unpacked）",
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

    const copy = path.join(work, apps[0]);
    const copied = run("cp", ["-R", app, copy]);
    if (copied.status !== 0) throw new Error(`复制 .app 失败：${copied.stderr}`);
    const binary = path.join(copy, "Contents", "MacOS", "Aether");
    record("复制后的 .app 可执行", existsSync(binary), binary);
    if (!existsSync(binary)) return;

    const launched = await launch(binary, path.join(work, "run"));
    evidence.metrics = {
      window_visible_ms: launched.window?.visibleMs ?? null,
      ui_ready_ms: launched.ui?.uiReadyMs ?? null,
      db_created: launched.db,
      exit: launched.exited,
      stderr_tail: launched.stderr(),
    };
    record("macOS 打包应用首窗可见", Number(launched.window?.visibleMs) > 0, `visibleMs=${launched.window?.visibleMs ?? "?"}`);
    record("macOS 打包应用 UI ready", Number(launched.ui?.uiReadyMs) > 0, `uiReadyMs=${launched.ui?.uiReadyMs ?? "?"}`);
    record("macOS 打包应用创建数据目录 DB", launched.db === true, launched.dataDir);
    record("macOS 打包应用退出后无残留进程", launched.exited !== null, JSON.stringify(launched.exited));
  } finally {
    run("hdiutil", ["detach", "-quiet", mount]);
    try {
      rmSync(work, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch {
      // 句柄滞后：保留临时目录。
    }
  }
}

let exitCode = 1;
try {
  await main();
} catch (error) {
  record("macOS 安装冒烟执行", false, error?.stack ?? String(error));
} finally {
  evidence.finished_at = new Date().toISOString();
  evidence.passed = checks.every((check) => check.skip || check.exit === check.expect);
  mkdirSync(outDir, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  writeFileSync(
    path.join(outDir, `m9-11-macos-smoke-${stamp}.json`),
    `${JSON.stringify(evidence, null, 2)}\n`,
    "utf8",
  );
  exitCode = summarize("m9-11-macos-smoke", checks);
}
process.exit(exitCode);
