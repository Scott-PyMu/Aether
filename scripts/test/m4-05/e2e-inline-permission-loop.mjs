/**
 * M4-05 E2E：真实 WebView 内联权限回环（v1.18 承接显式化）。
 *
 * 驱动链（真实 WebView2 + 真实 IPC + 真实事件桥 + 真实权限网关 + 注册清单加载的 Mock 适配器进程）：
 *   注册清单（AETHER_RUNTIME_REGISTRY_DIR = 测试构建包）→ 运行时选择器 →
 *   表单创建会话 → 发送 `permission-loop:<target>` → 审批卡渲染（原文/规范化对照）→
 *   点击「允许（一次）」→ `permission_resolve` → 适配器收到决议 → 工具终态 →
 *   run 终态经真实事件桥渲染完成。
 *
 * 证据：`AETHER_M4_05_REPORT` 行（三段：ask → allowed → complete）与
 * `AETHER_M4_05_EXIT`；由 `scripts/test/m4-05/verify-m4-05.mjs` 归档。
 *
 * 稳定性：WebView2 渲染器存在低概率初始化异常（探针窗口就绪但页面 JS 不回报）；
 * 失败时最多重试一次，重试前清理 WebView2 用户数据目录（`%LOCALAPPDATA%/dev.aether.desktop/EBWebView`）。
 * 重试仅为基础设施抖动兜底，不改变任何断言。
 *
 * 用法：node scripts/test/m4-05/e2e-inline-permission-loop.mjs
 *   [--skip-frontend-build] [--skip-rust-build] [--skip-bundle-build]
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, lineFramer, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

/** 整树回收（Windows `taskkill /T /F`；Unix 进程组回退单杀）。 */
function killTree(child) {
  if (!child || child.exitCode !== null) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    child.kill("SIGKILL");
  }
}

/** 清理 WebView2 用户数据目录（仅 Windows；best-effort）。 */
function resetWebviewData() {
  if (process.platform !== "win32") return;
  const base = process.env.LOCALAPPDATA;
  if (!base) return;
  const target = path.join(base, "dev.aether.desktop", "EBWebView");
  try {
    rmSync(target, { recursive: true, force: true });
    console.log(`[e2e] 已清理 WebView2 数据目录：${target}`);
  } catch (error) {
    console.error(`[e2e] 清理 WebView2 数据目录失败（忽略）：${error}`);
  }
}

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");
const skipBundleBuild = args.includes("--skip-bundle-build");

const REPORT_LINE = "AETHER_M4_05_REPORT";
const EXIT_LINE = "AETHER_M4_05_EXIT";
const MAX_ATTEMPTS = 2;

const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
const binary = path.join(repoRoot, "target", "debug", binaryName);
const tmpRoot = path.join(repoRoot, "scripts", "test", ".tmp", "m4-05");
const bundleDir = path.join(tmpRoot, "runtime-bundle");
const dataDir = path.join(tmpRoot, "data");
const locationFile = path.join(tmpRoot, "data-location.json");
// P0 策略矩阵：`fs.write` **工作区内 ask / 工作区外 deny**（M2-03 DoD1）。
// 未绑定工作区时权限基准 = 数据目录（lib.rs 启动口径），因此回环目标取数据目录内。
const targetFile = path.join(dataDir, "permission-target.txt");

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

/** 单次尝试：干净数据目录 → 启动 → 收集报告 → 等待退出（超时整树回收）。 */
async function attemptRun() {
  rmSync(dataDir, { recursive: true, force: true });
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(targetFile, "M4-05 inline permission loop target\n", "utf8");

  console.log(`\n$ ${binary}   # AETHER_E2E_M4_05_PROBE=1（内联权限回环）`);
  const app = spawn(binary, [], {
    env: {
      ...env,
      AETHER_E2E_M4_05_PROBE: "1",
      AETHER_RUNTIME_REGISTRY_DIR: bundleDir,
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: locationFile,
      AETHER_E2E_M4_05_TARGET: targetFile,
      AETHER_E2E_M4_05_RUNTIME: "mock",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const lines = [];
  const stdoutDone = (async () => {
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
    const timer = setTimeout(() => resolve(null), 300_000);
    app.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code ?? -1);
    });
  });
  const timedOut = exitCode === null;
  if (timedOut) killTree(app);
  await Promise.all([stdoutDone, stderrDone]);

  const reports = lines
    .filter((line) => line.startsWith(REPORT_LINE))
    .map((line) => JSON.parse(line.slice(REPORT_LINE.length).trim()));
  return { lines, reports, exitCode, timedOut };
}

let result = null;
try {
  if (!skipFrontendBuild) {
    const { command: pnpm, prefix } = pnpmCommand();
    const exit = run(pnpm, [...prefix, "--filter", "@aether/desktop", "build"], { env });
    if (exit !== 0) throw new Error(`前端构建失败（exit=${exit}）`);
  }

  if (!skipBundleBuild) {
    const exit = run(
      process.execPath,
      [path.join(repoRoot, "scripts", "ci", "build-runtime-bundles.mjs"), "--out", bundleDir],
      { env: { ...env, AETHER_BUNDLE_MOCK: "1" } },
    );
    if (exit !== 0) throw new Error(`运行时包构建失败（exit=${exit}）`);
  }
  if (!existsSync(path.join(bundleDir, "runtimes.json"))) {
    throw new Error(`未找到注册清单 ${path.join(bundleDir, "runtimes.json")}`);
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

  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
    if (attempt > 1) {
      console.warn("[e2e] 上次尝试未进入运行时选择（页面未回报），清理 WebView2 数据目录后重试一次");
      resetWebviewData();
    }
    result = await attemptRun();
    const reached = result.reports.some((report) => report.stage === "runtime-selected");
    if (reached) break;
    if (attempt < MAX_ATTEMPTS) {
      console.warn(
        `[e2e] 尝试 ${attempt}/${MAX_ATTEMPTS} 未达运行时选择（timeout=${result.timedOut} exit=${result.exitCode}），准备重试`,
      );
    }
  }

  const { lines, reports, exitCode, timedOut } = result;
  const byStage = (stage) => reports.find((report) => report.stage === stage);

  const listenProbe = byStage("event-listen-probe");
  record(
    "真实 WebView 事件通道监听注册成功（core:event:allow-listen；ADR-016）",
    Boolean(listenProbe && listenProbe.ok === true),
    listenProbe ? (listenProbe.ok ? "ok" : listenProbe.error) : "缺少 event-listen-probe 回报",
  );
  const unlistenProbe = byStage("event-unlisten-probe");
  record(
    "真实 WebView 事件通道解除监听成功（core:event:allow-unlisten；ADR-016 §6-B1）",
    Boolean(unlistenProbe && unlistenProbe.ok === true),
    unlistenProbe ? (unlistenProbe.ok ? "ok" : unlistenProbe.error) : "缺少 event-unlisten-probe 回报",
  );

  const selected = byStage("runtime-selected");
  record(
    "注册清单加载：运行时选择器渲染已注册运行时（含 mock）",
    Boolean(selected && Array.isArray(selected.available) && selected.available.includes("mock")),
    selected ? (selected.available ?? []).join(",") : "缺少 runtime-selected 回报",
  );
  record("会话已发送 permission-loop 触发", Boolean(byStage("sent")));

  const ask = byStage("ask");
  record(
    "真实 WebView 渲染审批卡（permission-card + 原文/规范化对照）",
    Boolean(ask && ask.card === true && typeof ask.target_raw === "string" && ask.target_raw.length > 0),
    ask ? `raw=${ask.target_raw}` : "缺少 ask 回报",
  );
  record(
    "审批卡目标与注入目标一致",
    Boolean(ask && ask.target_raw && ask.target_raw.includes(path.basename(targetFile))),
    ask ? ask.target_raw : "",
  );

  const allowed = byStage("allowed");
  record("点击「允许（一次）」触发 permission_resolve", Boolean(allowed && allowed.scope === "once"));

  const complete = byStage("complete");
  record(
    "适配器收到决议并完成 run（工具终态 + run 终态经事件桥渲染）",
    Boolean(complete && complete.tool_status === "completed" && complete.bubbles >= 2),
    complete
      ? `tool=${complete.tool_status} bubbles=${complete.bubbles}`
      : "缺少 complete 回报",
  );
  const exitLine = lines.find((line) => line.startsWith(EXIT_LINE));
  record("探针进程退出码为 0", exitCode === 0, `实际 ${exitCode}`);
  record(
    "探针正常终止（terminal）",
    Boolean(exitLine && exitLine.includes('"terminal"')),
    timedOut ? "超时（含重试后）" : undefined,
  );
} catch (error) {
  record("E2E 执行", false, String(error && error.message ? error.message : error));
} finally {
  rmSync(targetFile, { force: true });
}

process.exit(summarize("m4-05-inline-permission-loop", checks));
