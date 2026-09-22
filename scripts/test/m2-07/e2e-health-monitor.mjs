/**
 * M2-07 E2E：UI 健康轮询（DoD4/DoD5；D2 / ADR-007 附录 A.3）。
 *
 * 在真实 WebView2 中驱动前端 `HealthMonitor`（`apps/desktop/src/useHealthPolling.ts`）：
 *   1) observe（正常库）：每 5s 轮询 `health` → 渲染 `health-normal`；
 *   2) observe（损坏库）：核心启动失败按 `persist_degraded` 呈现 → 渲染只读降级横幅；
 *      同时断言日志汇聚端产物 `<data_dir>/logs/aether.log` 含启动失败诊断（M2-07 DoD6）；
 *   3) stall：注入 `health` 调用挂起（拦截 `__TAURI_INTERNALS__.invoke`）→ 15s 后
 *      渲染 `core-unresponsive` + `core-restart`（重启入口）。
 *
 * 用法：node scripts/test/m2-07/e2e-health-monitor.mjs [--skip-frontend-build] [--skip-rust-build]
 * 仅 Windows（WebView2 宿主）；CI 由 security-baseline job（windows-2022）执行。
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, lineFramer, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");

const REPORT_LINE = "AETHER_M2_07_REPORT";

const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
const binary = path.join(repoRoot, "target", "debug", binaryName);

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

/** 启动一次探针并等待退出，返回 stdout 行与退出码。 */
async function launchProbe(dataDir, mode, timeoutMs) {
  console.log(`\n$ ${binary}   # AETHER_E2E_HEALTH_PROBE=1 MODE=${mode} DATA=${dataDir}`);
  const app = spawn(binary, [], {
    env: {
      ...env,
      AETHER_DATA_DIR: dataDir,
      AETHER_E2E_HEALTH_PROBE: "1",
      AETHER_E2E_HEALTH_MODE: mode,
      // stall：`health` 命令在运行时上挂起（观察期内不返回；不阻塞主线程）。
      ...(mode === "stall" ? { AETHER_E2E_HEALTH_STALL_MS: "600000" } : {}),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const lines = [];
  const stdoutDone = (async () => {
    // 行框定：跨 chunk 缓冲未闭合行（管道分片会截断 REPORT JSON → 回报丢失）。
    const framer = lineFramer((line) => {
      lines.push(line);
      console.log(`[app] ${line}`);
    });
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
  return { lines, exitCode };
}

/** 解析 `AETHER_M2_07_REPORT <json>` 行。 */
function reportsOf(lines) {
  return lines
    .filter((line) => line.startsWith(REPORT_LINE))
    .map((line) => {
      try {
        return JSON.parse(line.slice(REPORT_LINE.length).trim());
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

const tempRoot = mkdtempSync(path.join(os.tmpdir(), "aether-m2-07-e2e-"));
const normalDir = path.join(tempRoot, "normal");
const degradedDir = path.join(tempRoot, "degraded");
const stallDir = path.join(tempRoot, "stall");
const dirs = [normalDir, degradedDir, stallDir];
for (const dir of dirs) {
  mkdirSync(dir, { recursive: true });
}
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

  // ===== 1. observe（正常库）→ health-normal（每 5s 轮询） =====
  const normal = await launchProbe(normalDir, "observe", 90_000);
  const normalReport = reportsOf(normal.lines).find((value) => value.stage === "normal");
  record(
    "正常态：UI 轮询 health 并渲染 health-normal（storage_state=normal）",
    Boolean(normalReport) && normalReport.storage_state === "normal",
    normalReport ? JSON.stringify(normalReport) : "缺少 stage=normal 回报",
  );
  record("正常态：探针进程退出码 0", normal.exitCode === 0, `实际 ${normal.exitCode}`);
  const normalLog = path.join(normalDir, "logs", "aether.log");
  record(
    "日志汇聚端接线：正常态产物 <data_dir>/logs/aether.log 存在（M2-07 DoD6）",
    existsSync(normalLog),
    normalLog,
  );

  // ===== 2. observe（损坏库）→ 只读降级横幅 + 启动失败诊断入日志汇聚端 =====
  const degraded = await launchProbe(degradedDir, "observe", 90_000);
  const degradedReport = reportsOf(degraded.lines).find((value) => value.stage === "degraded");
  record(
    "降级态：UI 渲染 storage-degraded 只读横幅（storage_state=persist_degraded）",
    Boolean(degradedReport) && degradedReport.storage_state === "persist_degraded",
    degradedReport ? JSON.stringify(degradedReport) : "缺少 stage=degraded 回报",
  );
  record("降级态：探针进程退出码 0", degraded.exitCode === 0, `实际 ${degraded.exitCode}`);
  const degradedLog = path.join(degradedDir, "logs", "aether.log");
  const degradedLogText = existsSync(degradedLog) ? readFileSync(degradedLog, "utf8") : "";
  record(
    "日志汇聚端捕获启动失败诊断（核心健康源启动失败 / persist_degraded 可经诊断包导出）",
    degradedLogText.includes("核心健康源启动失败"),
    degradedLogText.split(/\r?\n/).find((line) => line.includes("核心健康源启动失败")) ?? "未捕获",
  );

  // ===== 3. stall：health 挂起 15s → 核心未响应 + 重启入口 =====
  const stall = await launchProbe(stallDir, "stall", 120_000);
  const stallReport = reportsOf(stall.lines).find((value) => value.stage === "unresponsive");
  record(
    "无响应态：15s 无 health 响应 → core-unresponsive + core-restart 同时可达",
    Boolean(stallReport) && stallReport.restart === true,
    stallReport ? JSON.stringify(stallReport) : "缺少 stage=unresponsive 回报",
  );
  record("无响应态：探针进程退出码 0", stall.exitCode === 0, `实际 ${stall.exitCode}`);
} catch (error) {
  record("E2E 执行", false, String(error && error.message ? error.message : error));
} finally {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
  rmSync(tempRoot, { recursive: true, force: true });
}

process.exit(summarize("m2-07-health-monitor-e2e", checks));
