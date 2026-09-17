/**
 * M1-06 真实系统选择器手动冒烟（半自动；Windows 桌面会话）。
 *
 * 与 E2E（`e2e-startup-guard.mjs`，注入 FixedDirectoryPicker）互补：
 * 本脚本不注入选择器，驱动真实 Tauri dialog：
 *   1. 启动应用（拒绝启动门）→ 探针点击「选择目录…」→ 原生目录对话框打开；
 *   2. 截取前台窗口（对话框）截图归档；
 *   3. 通过 WScript.Shell 模拟键入目标路径 + 回车完成选择；
 *   4. 断言页面输入框被回填为目标路径（`stage":"picked"` 回报）。
 *
 * 用法：
 *   node scripts/test/m1-06/manual-picker-smoke.mjs [--skip-frontend-build] [--skip-rust-build]
 *       [--non-interactive]   # 仅打开对话框 + 截图 + ESC 取消（无按键自动化环境）
 *
 * 证据归档：scripts/test/.tmp/m1-06-picker-smoke/<stamp>/
 *   （app.log、picker-dialog*.png、smoke-result.json；截图 sha256 记录在结果内）
 */
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");
const nonInteractive = args.includes("--non-interactive");
const dumpUia = args.includes("--dump-uia");

const PHASE_LINE = "AETHER_M1_06_PHASE";
const REPORT_LINE = "AETHER_M1_06_REPORT";

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) =>
  checks.push({ name: detail ? `${name}（${detail}）` : name, ok });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const fileSha256 = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");

if (process.platform !== "win32") {
  console.error("[picker-smoke] 真实系统选择器冒烟仅支持 Windows（WScript.Shell 按键自动化）");
  process.exit(2);
}

const binaryName = "aether-tauri.exe";
const binary = path.join(repoRoot, "target", "debug", binaryName);
const frontendDist = path.join(repoRoot, "apps", "desktop", "dist");
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const workDir = path.join(repoRoot, "scripts", "test", ".tmp", "m1-06-picker-smoke", stamp);
const sourceDir = path.join(workDir, "OneDrive-sim", "Aether");
const targetDir = path.join(workDir, "local-target");
const pointerFile = path.join(workDir, "config", "data-location.json");

function launch(extraEnv) {
  const child = spawn(binary, [], {
    env: { ...env, ...extraEnv },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const state = { lines: [], stderr: [], exited: false, code: null };
  child.stdout.on("data", (chunk) => {
    for (const line of String(chunk).split(/\r?\n/)) {
      if (!line.trim()) continue;
      state.lines.push(line);
      console.log(`[app] ${line}`);
    }
  });
  child.stderr.on("data", (chunk) => {
    const text = String(chunk);
    state.stderr.push(text.trimEnd());
    console.error(`[app:stderr] ${text.trimEnd()}`);
  });
  const exit = new Promise((resolve) => {
    child.once("exit", (code) => {
      state.exited = true;
      state.code = code ?? -1;
      resolve(state.code);
    });
  });
  return { child, state, exit };
}

async function waitFor(state, predicate, timeoutMs, description) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const match = state.lines.find(predicate);
    if (match) return match;
    if (state.exited) break;
    await sleep(200);
  }
  const late = state.lines.find(predicate);
  if (late) return late;
  throw new Error(`等待超时：${description}（stdout ${state.lines.length} 行）`);
}

function powershell(script, scriptArgs) {
  return spawnSync(
    "powershell.exe",
    ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script, ...scriptArgs],
    { encoding: "utf8" },
  );
}

const captureScript = path.join(repoRoot, "scripts", "test", "m1-06", "picker-capture.ps1");
const uiaScript = path.join(repoRoot, "scripts", "test", "m1-06", "picker-uia.ps1");

/// 截取前台窗口并返回其进程号（PID 与 App 一致才认定对话框在前台）。
function captureForeground(label) {
  const outFile = path.join(workDir, `${label}.png`);
  const result = powershell(captureScript, [outFile]);
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim();
  const ok = result.status === 0 && existsSync(outFile);
  const pidMatch = /pid=(\d+)/.exec(result.stdout ?? "");
  const pid = pidMatch ? Number(pidMatch[1]) : null;
  record(`截图（${label}）`, ok, ok ? `${outFile} pid=${pid}` : output);
  return { file: ok ? outFile : null, pid };
}

/// UI Automation 操作对话框（不依赖焦点、不产生全局按键）：
/// select = 设置「文件夹:」编辑框值并点「选择文件夹」；cancel = 点「取消」。
function uiaAction(action, logName, targetPath = "") {
  const args = [
    "-AppPid",
    String(app?.child?.pid ?? 0),
    "-Action",
    action,
    "-OutLog",
    path.join(workDir, logName),
  ];
  if (targetPath) args.push("-Path", targetPath);
  return powershell(uiaScript, args);
}

let app = null;
try {
  // -------------------------------------------------------------------------
  // 0. 构建前端与壳层
  // -------------------------------------------------------------------------
  if (!skipFrontendBuild) {
    const { command: pnpm, prefix } = pnpmCommand();
    const exit = run(pnpm, [...prefix, "--filter", "@aether/desktop", "build"], { env });
    if (exit !== 0) throw new Error(`前端构建失败（exit=${exit}）`);
  }
  if (!existsSync(path.join(frontendDist, "index.html"))) {
    throw new Error(`未找到前端产物 ${frontendDist}/index.html`);
  }
  if (!skipRustBuild) {
    const exit = run(
      bin("cargo"),
      ["build", "-p", "aether-tauri", "--features", "custom-protocol"],
      { env },
    );
    if (exit !== 0) throw new Error(`cargo build 失败（exit=${exit}）`);
  }
  if (!existsSync(binary)) throw new Error(`未找到壳层产物 ${binary}`);

  // -------------------------------------------------------------------------
  // 1. 夹具与启动
  // -------------------------------------------------------------------------
  mkdirSync(sourceDir, { recursive: true });
  mkdirSync(targetDir, { recursive: true });
  mkdirSync(path.dirname(pointerFile), { recursive: true });
  writeFileSync(path.join(sourceDir, "aether.db"), "aether-db-bytes");
  console.log(`[picker-smoke] 工作目录：${workDir}`);
  console.log(`[picker-smoke] 目标目录：${targetDir}`);

  app = launch({
    OneDrive: path.join(workDir, "OneDrive-sim"),
    AETHER_DATA_DIR: sourceDir,
    AETHER_DATA_LOCATION_FILE: pointerFile,
    AETHER_E2E_STARTUP_PROBE: "1",
    AETHER_E2E_PICKER_SMOKE: "1",
  });

  await waitFor(
    app.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"blocked"'),
    240000,
    "拒绝启动界面就绪",
  );
  record("拒绝启动门可达（真实运行形态）", true);

  const openLine = await waitFor(
    app.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"picker-open"'),
    120000,
    "点击「选择目录…」触发原生对话框",
  );
  record("「选择目录…」触发原生对话框", Boolean(openLine));
  await sleep(1500);

  captureForeground("picker-dialog");

  if (dumpUia) {
    const dump = uiaAction("dump", "uia-dump.json");
    const dumpFile = path.join(workDir, "uia-dump.json");
    if (existsSync(dumpFile)) {
      console.log(readFileSync(dumpFile, "utf8"));
    } else {
      console.error(`UIA dump 失败：${(dump.stderr ?? "").trim()}`);
    }
    uiaAction("cancel", "uia-cancel.json");
    record("UIA 树导出（诊断）", existsSync(dumpFile));
  } else if (nonInteractive) {
    const cancel = uiaAction("cancel", "uia-cancel.json");
    record("非交互模式：取消对话框（UIA）", cancel.status === 0);
    await sleep(800);
    record("非交互模式：跳过选择（仅冒烟对话框与截图）", true, "skipped");
  } else {
    const waitPicked = async (timeoutMs) => {
      try {
        const line = await waitFor(
          app.state,
          (item) => item.startsWith(REPORT_LINE) && item.includes('"stage":"picked"'),
          timeoutMs,
          "对话框选择回填输入框",
        );
        return JSON.parse(line.slice(REPORT_LINE.length).trim()).value;
      } catch {
        return null;
      }
    };

    // UIA 定位对话框 + 前台校验 + AttachThreadInput 聚焦「文件夹:」编辑框 →
    // 剪贴板粘贴路径 + 回车（仅在对话框确认位于前台时注入键盘事件）。
    const select = uiaAction("select", "uia-select.json", targetDir);
    record("真实选择器完成选择（前台校验 + 键盘注入）", select.status === 0, (select.stderr ?? "").trim());

    // 文件夹选择器：回车通常先导航到路径；若对话框仍打开，再点「选择文件夹」提交。
    let value = await waitPicked(6000);
    if (value === null) {
      const accept = uiaAction("accept", "uia-accept.json");
      record("导航后点击「选择文件夹」提交", accept.status === 0, (accept.stderr ?? "").trim());
      value = await waitPicked(15000);
    }
    record("真实选择器选择结果回填输入框", value === targetDir, `value=${value}`);
    if (value !== targetDir) {
      captureForeground("picker-dialog-timeout");
      uiaAction("cancel", "uia-cancel.json");
    }
  }

  record(
    "启动门快照输出（探针）",
    app.state.lines.some((line) => line.startsWith(PHASE_LINE)),
  );
} catch (error) {
  record("冒烟执行", false, String(error && error.message ? error.message : error));
} finally {
  if (app && !app.state.exited) app.child.kill();
}

// ---------------------------------------------------------------------------
// 2. 证据归档
// ---------------------------------------------------------------------------
const screenshotHashes = {};
for (const name of ["picker-dialog.png", "picker-dialog-timeout.png"]) {
  const file = path.join(workDir, name);
  if (existsSync(file)) {
    screenshotHashes[name] = fileSha256(file);
  }
}
const result = {
  stamp,
  workDir,
  targetDir,
  nonInteractive,
  checks,
  screenshotHashes,
  appStdout: app ? app.state.lines : [],
  appStderr: app ? app.state.stderr : [],
};
writeFileSync(path.join(workDir, "smoke-result.json"), `${JSON.stringify(result, null, 2)}\n`);
writeFileSync(
  path.join(workDir, "app.log"),
  `${result.appStdout.join("\n")}\n--- stderr ---\n${result.appStderr.join("\n")}\n`,
);

console.log(`\n===== m1-06-picker-smoke =====`);
let failed = 0;
for (const check of checks) {
  if (!check.ok) failed += 1;
  console.log(`${check.ok ? "PASS" : "FAIL"}  ${check.name}`);
}
console.log(`----- ${failed === 0 ? "全部通过" : `${failed} 项失败`} -----`);
console.log(`[picker-smoke] 证据归档：${workDir}`);
process.exitCode = failed === 0 ? 0 : 1;
