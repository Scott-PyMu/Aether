#!/usr/bin/env node
/**
 * M9-11 Linux 安装产物冒烟（DoD-6，CI runner 执行）。
 *
 * 不要求先安装到系统：优先解包 AppImage（`--appimage-extract`），否则 `dpkg-deb -x`
 * 解包 deb；在 `xvfb-run` 下启动打包后的 Electron 应用，断言：
 *  - `resources/bin/aetherd` + `runtime-bundle`（共享宿主 + 三官方运行时）；
 *  - 首窗/daemon ready/UI ready（shell.log 计时）；
 *  - 数据目录 DB 初始化；进程树可回收。
 *
 * 用法：node scripts/test/m9-11/smoke-linux.mjs [--appimage <path>] [--deb <path>]
 */
import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
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
  schema: "aether.m9-11.linux-smoke.v1",
  platform: process.platform,
  started_at: new Date().toISOString(),
  checks,
};

function argValue(flag) {
  const index = process.argv.indexOf(flag);
  return index >= 0 ? path.resolve(process.argv[index + 1]) : null;
}

function findBySuffix(suffix) {
  if (!existsSync(outDir)) return null;
  const files = readdirSync(outDir)
    .filter((name) => name.startsWith("Aether-") && name.endsWith(suffix))
    .sort();
  return files.length === 0 ? null : path.join(outDir, files[files.length - 1]);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    timeout: options.timeout ?? 300_000,
    cwd: options.cwd,
  });
  if (result.error) throw new Error(`${command} 无法执行：${result.error.message}`);
  return result;
}

function findFile(root, name) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      const nested = findFile(full, name);
      if (nested) return nested;
    } else if (entry.name === name) {
      return full;
    }
  }
  return null;
}

function unpack(appImage, deb, work) {
  if (appImage && existsSync(appImage)) {
    chmodSync(appImage, 0o755);
    const extract = run(appImage, ["--appimage-extract"], { cwd: work, timeout: 180_000 });
    if (extract.status !== 0) throw new Error(`AppImage extract 失败：${extract.stderr}`);
    const root = path.join(work, "squashfs-root");
    const binary = [
      path.join(root, "aether"),
      path.join(root, "AppRun"),
    ].find((candidate) => existsSync(candidate));
    if (!binary) throw new Error("AppImage 内未找到 aether/AppRun");
    chmodSync(binary, 0o755);
    return { kind: "appimage", root, binary, artifact: appImage };
  }
  if (deb && existsSync(deb)) {
    const root = path.join(work, "deb-root");
    mkdirSync(root, { recursive: true });
    const extract = run("dpkg-deb", ["-x", deb, root], { timeout: 120_000 });
    if (extract.status !== 0) throw new Error(`dpkg-deb 解包失败：${extract.stderr}`);
    const binary = findFile(root, "aether");
    if (!binary) throw new Error("deb 内未找到 aether 可执行文件");
    chmodSync(binary, 0o755);
    return { kind: "deb", root, binary, artifact: deb };
  }
  throw new Error("未找到 AppImage/deb 产物");
}

async function launch(binary, root) {
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user-data");
  const home = path.join(root, "home");
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  mkdirSync(path.join(home, "Downloads"), { recursive: true });
  const child = spawn(
    "xvfb-run",
    ["-a", "-s", "-screen 0 1280x800x24", binary, "--no-sandbox"],
    {
      cwd: path.dirname(binary),
      env: {
        ...process.env,
        HOME: home,
        AETHER_DATA_DIR: dataDir,
        AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
        AETHER_USER_DATA_DIR: userData,
        // Linux CI 无 Secret Service：跳过 S-13 弹窗（DoD-6 只做构建/启动冒烟；
        // Keychain 门禁在 M9-09/G5 的 Windows/macOS 证据中覆盖）。
        AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
      detached: true,
    },
  );
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
    result.window = await waitEvent("window.ready", 30_000);
    result.ui = await waitEvent("ui.ready", 45_000);
    result.db = existsSync(path.join(dataDir, "aether.db"));
  } finally {
    await sleep(1_500);
    if (child.exitCode === null) {
      try {
        process.kill(-child.pid, "SIGTERM");
      } catch {
        child.kill("SIGTERM");
      }
      await waitFor(() => result.exited !== null, 10_000, "Linux 应用退出").catch(() => {});
    }
    if (child.exitCode === null) {
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {
        child.kill("SIGKILL");
      }
    }
  }
  return result;
}

async function main() {
  if (process.platform !== "linux") {
    console.error("[m9-11-linux] 仅 Linux 运行（CI ubuntu runner）。");
    process.exit(2);
  }
  const appImage = argValue("--appimage") ?? findBySuffix(".AppImage");
  const deb = argValue("--deb") ?? findBySuffix(".deb");
  record("找到 Linux 安装产物", Boolean(appImage || deb), appImage ?? deb ?? "未找到");
  if (!appImage && !deb) return;

  const work = mkdtempSync(path.join(outDir, "m9-11-linux-"));
  try {
    const unpacked = unpack(appImage, deb, work);
    evidence.artifact = {
      kind: unpacked.kind,
      path: path.relative(repoRoot, unpacked.artifact),
      bytes: statSync(unpacked.artifact).size,
    };
    record(`解包 ${unpacked.kind} 并定位应用`, true, path.relative(work, unpacked.binary));

    const resources = path.join(path.dirname(unpacked.binary), "resources");
    const daemon = path.join(resources, "bin", "aetherd");
    const manifestPath = path.join(resources, "runtime-bundle", "runtimes.json");
    record("extraResources: resources/bin/aetherd", existsSync(daemon), daemon);
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
    const asar = path.join(resources, "app.asar");
    record("resources/app.asar 存在", existsSync(asar), asar);
    if (existsSync(daemon)) {
      const version = spawnSync(daemon, ["--version"], { encoding: "utf8", timeout: 30_000 });
      record(
        "安装产物 aetherd --version 可执行",
        version.status === 0 && /\d+\.\d+\.\d+/.test(`${version.stdout}${version.stderr}`),
        `${version.stdout ?? ""}`.trim().split(/\r?\n/)[0] ?? "",
      );
    }

    const launched = await launch(unpacked.binary, work);
    evidence.metrics = {
      window_visible_ms: launched.window?.visibleMs ?? null,
      ui_ready_ms: launched.ui?.uiReadyMs ?? null,
      db_created: launched.db,
      exit: launched.exited,
      stderr_tail: launched.stderr(),
    };
    record(
      "Linux 打包应用首窗可见",
      Number(launched.window?.visibleMs) > 0,
      `visibleMs=${launched.window?.visibleMs ?? "?"}`,
    );
    record(
      "Linux 打包应用 UI ready",
      Number(launched.ui?.uiReadyMs) > 0,
      `uiReadyMs=${launched.ui?.uiReadyMs ?? "?"}`,
    );
    record("Linux 打包应用创建数据目录 DB", launched.db === true, launched.dataDir);
    record("Linux 打包应用退出后无残留进程", launched.exited !== null, JSON.stringify(launched.exited));
  } finally {
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
  record("Linux 安装冒烟执行", false, error?.stack ?? String(error));
} finally {
  evidence.finished_at = new Date().toISOString();
  evidence.passed = checks.every((check) => check.skip || check.exit === check.expect);
  mkdirSync(outDir, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  writeFileSync(
    path.join(outDir, `m9-11-linux-smoke-${stamp}.json`),
    `${JSON.stringify(evidence, null, 2)}\n`,
    "utf8",
  );
  exitCode = summarize("m9-11-linux-smoke", checks);
}
process.exit(exitCode);
