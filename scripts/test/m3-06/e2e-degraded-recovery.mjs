/**
 * M3-06 E2E：存储降级恢复引导（DoD3；D4/ADR-004「无热恢复」口径）。
 *
 * 在真实 WebView2 中驱动前端（`HealthMonitor` 降级横幅 + `SessionWorkbench` 发送入口）：
 *   1) 损坏库 → 核心启动失败按 `persist_degraded` 呈现 → UI 渲染只读降级横幅：
 *      `storage-degraded-restart`（app_restart）可达、发送入口禁用、恢复引导文案、
 *      无「一键恢复/热恢复」入口；
 *   2) 点击重启 → `app_restart`（命令层 `AppHandle::request_restart`）→ 复用 D2 关闭
 *      序列 → 进程重启并重跑启动序列（A4/quick_check/孤儿清理/重启状态重建）→
 *      新进程再次渲染降级横幅并回报（证明启动自检完成，且不自动热恢复）。
 *
 * 重启进程继承同一 stdout 管道（`Command::spawn` 默认继承 stdio），脚本在同一流上
 * 等待 `AETHER_M3_06_REPORT {stage:"restarted"}`。
 *
 * 用法：node scripts/test/m3-06/e2e-degraded-recovery.mjs [--skip-frontend-build] [--skip-rust-build]
 * 仅 Windows（WebView2 宿主）；CI 由 security-baseline job（windows-2022）执行。
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, lineFramer, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");

const REPORT_LINE = "AETHER_M3_06_REPORT";

const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
const binary = path.join(repoRoot, "target", "debug", binaryName);

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

/** 启动一次降级恢复探针并等待「重启后回报 + 退出」，返回 stdout 行与退出码。 */
async function launchProbe(dataDir, timeoutMs) {
  console.log(`\n$ ${binary}   # AETHER_E2E_M3_06_PROBE=1 DATA=${dataDir}`);
  const app = spawn(binary, [], {
    env: {
      ...env,
      AETHER_DATA_DIR: dataDir,
      AETHER_E2E_M3_06_PROBE: "1",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const firstPid = app.pid;
  const lines = [];
  const stdoutDone = (async () => {
    const framer = lineFramer((line) => {
      lines.push(line);
      console.log(`[app] ${line}`);
    });
    // 重启进程继承同一管道：流在第一进程退出后仍保持，直到重启进程退出。
    for await (const chunk of app.stdout) framer.feed(chunk);
    framer.flush();
  })();
  const stderrDone = (async () => {
    for await (const chunk of app.stderr) {
      console.error(`[app:stderr] ${String(chunk).trimEnd()}`);
    }
  })();
  const exitCode = await new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    app.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code ?? -1);
    });
  });
  if (exitCode === null) {
    app.kill();
  }
  await Promise.all([stdoutDone, stderrDone]);
  return { lines, exitCode, firstPid };
}

/** 解析 `AETHER_M3_06_REPORT <json>` 行（按标记子串定位，容忍前缀）。 */
function reportsOf(lines, stage) {
  return lines
    .filter((line) => line.includes(REPORT_LINE))
    .map((line) => {
      const index = line.indexOf(REPORT_LINE);
      try {
        return JSON.parse(line.slice(index + REPORT_LINE.length).trim());
      } catch {
        return null;
      }
    })
    .filter(Boolean)
    .filter((value) => value.stage === stage);
}

const tempRoot = mkdtempSync(path.join(os.tmpdir(), "aether-m3-06-e2e-"));
const degradedDir = path.join(tempRoot, "degraded");
mkdirSync(degradedDir, { recursive: true });
// 损坏库：核心启动（quick_check）失败 → degraded_backend（persist_degraded）。
writeFileSync(path.join(degradedDir, "aether.db"), "not a database");

try {
  if (!skipFrontendBuild) {
    const { command: pnpm, prefix } = pnpmCommand();
    const exit = run(pnpm, [...prefix, "--filter", "@aether/desktop", "build"], { env });
    if (exit !== 0) throw new Error(`前端构建失败（exit=${exit}）`);
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

  const probe = await launchProbe(degradedDir, 240_000);
  const degraded = reportsOf(probe.lines, "degraded")[0];
  record(
    "降级态：只读横幅可达 + 重启入口（storage-degraded-restart）+ 恢复引导",
    Boolean(degraded) && degraded.restart_button === true && degraded.hot_recovery === false,
    degraded ? JSON.stringify(degraded) : "缺少 stage=degraded 回报",
  );
  record(
    "降级态：发送入口禁用 + 降级提示（composer-disabled-hint）",
    Boolean(degraded) &&
      degraded.composer_disabled === true &&
      typeof degraded.hint === "string" &&
      degraded.hint.includes("存储降级"),
    degraded ? String(degraded.hint) : "缺少 hint",
  );

  const restarted = reportsOf(probe.lines, "restarted")[0];
  record(
    "重启后：新进程重跑启动序列并再次渲染降级横幅（app_restart 生效）",
    Boolean(restarted) &&
      typeof restarted.pid === "number" &&
      restarted.pid !== probe.firstPid,
    restarted
      ? `first_pid=${probe.firstPid} restarted_pid=${restarted.pid}`
      : "缺少 stage=restarted 回报",
  );
  record(
    "重启后：探针进程正常退出（exit=0）",
    probe.exitCode === 0,
    `实际 ${probe.exitCode}`,
  );
} catch (error) {
  record("E2E 执行", false, String(error && error.message ? error.message : error));
} finally {
  rmSync(tempRoot, { recursive: true, force: true });
}

process.exit(summarize("m3-06-degraded-recovery-e2e", checks));
