#!/usr/bin/env node
/**
 * M9-13 DoD-5：性能硬阈值证据（ADR-030；P2 计划 v0.6 §2.13/§4）。
 *
 * 采集：
 *  - Electron + 真实 `aetherd` + Mock runtime-bundle（M9-12 前置产物）：
 *    窗口可见 / initialize 握手 / UI ready / 会话创建 / idle RSS / 10 会话 RSS；
 *  - M9-05 事件桥 10 会话 × 50 delta/s：控制事件 0 丢失、P95 <150ms；
 *  - 安装包体积（dist-out 最新 MSI/DMG，硬阈值 ≤1.5× HEAD 且 ≤450MB）；
 *  - 关闭后 WAL 大小（drain → checkpoint）。
 *
 * 输出：`scripts/test/.tmp/m9-13/performance/performance.json`。
 *
 * 前置：
 *   pnpm build:desktop
 *   pnpm prepare:m9-12:e2e   # Mock bundle + aetherd（test-hooks；M9-13 复用）
 *
 * 用法：node scripts/test/m9-13/performance.mjs [--skip-10x50]
 */
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
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

import { resolveElectronExecutable } from "../../ci/electron-bin.mjs";
import { pnpmCommand } from "../lib/exec.mjs";
import { measureProcessTree } from "./lib/process-tree.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const appRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(appRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");
const electronPath = resolveElectronExecutable({ repoRoot });
const daemonPath = path.join(
  repoRoot,
  "target",
  "debug",
  process.platform === "win32" ? "aetherd.exe" : "aetherd",
);
const bundleDir = path.resolve(
  repoRoot,
  process.env.AETHER_M9_12_BUNDLE ?? path.join("dist-out", "m9-12-runtime-bundle"),
);
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-13", "performance");
const outPath = path.join(outDir, "performance.json");
const thresholds = JSON.parse(
  readFileSync(path.join(repoRoot, "docs/M9-01-baseline/freeze/thresholds.json"), "utf8"),
);
const skip10x50 = process.argv.includes("--skip-10x50");
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

mkdirSync(outDir, { recursive: true });

function fail(message) {
  console.error(`[m9-13-performance] FAIL ${message}`);
  process.exit(1);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let last;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      last = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${last ? `（${last.message}）` : ""}`);
}

function killTree(pid) {
  if (!pid || pid <= 0) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
      stdio: "ignore",
      timeout: 10_000,
    });
  } else {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // 已退出。
      }
    }
  }
}

function latestInstaller() {
  const dir = path.join(repoRoot, "dist-out");
  if (!existsSync(dir)) return null;
  const names = readdirSync(dir)
    .filter((name) => /^Aether.*\.(msi|dmg)$/i.test(name))
    .map((name) => {
      const full = path.join(dir, name);
      return { path: full, bytes: statSync(full).size, mtime: statSync(full).mtimeMs };
    })
    .sort((left, right) => right.mtime - left.mtime);
  return names[0] ?? null;
}

function latestSmokeEvidence() {
  const dir = path.join(repoRoot, "dist-out");
  if (!existsSync(dir)) return null;
  const files = readdirSync(dir)
    .filter((name) => name.startsWith("m9-11-smoke-test-") && name.endsWith(".json"))
    .sort();
  if (files.length === 0) return null;
  const file = path.join(dir, files[files.length - 1]);
  try {
    return { file, value: JSON.parse(readFileSync(file, "utf8")) };
  } catch {
    return null;
  }
}

function runSmokeInstaller() {
  if (process.argv.includes("--skip-smoke")) {
    const existing = latestSmokeEvidence();
    return existing
      ? { skipped: false, reused: true, ...existing }
      : { skipped: true, reason: "--skip-smoke 且无既有 m9-11-smoke-test-*.json" };
  }
  const pnpm = pnpmCommand();
  const tmpDir = path.join(repoRoot, "dist-out", "tmp");
  mkdirSync(tmpDir, { recursive: true });
  console.log("[m9-13-performance] 运行安装产物冒烟（M9-11 test MSI；窗口/UI ready/idle RSS）");
  const result = spawnSync(
    pnpm.command,
    [...pnpm.prefix, "smoke:installer", "--variant", "test"],
    {
      cwd: repoRoot,
      encoding: "utf8",
      env: {
        ...process.env,
        CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "1",
        TEMP: tmpDir,
        TMP: tmpDir,
      },
      timeout: 20 * 60 * 1000,
      maxBuffer: 64 * 1024 * 1024,
    },
  );
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  if (result.status !== 0) {
    return {
      skipped: false,
      passed: false,
      exit: result.status,
      output_tail: output.split(/\r?\n/).slice(-40).join("\n"),
    };
  }
  const evidence = latestSmokeEvidence();
  if (!evidence) {
    return { skipped: false, passed: false, reason: "冒烟通过但未找到 m9-11-smoke-test-*.json" };
  }
  return { skipped: false, passed: evidence.value?.passed === true, ...evidence };
}

function run10x50() {
  if (skip10x50) {
    return { skipped: true, reason: "--skip-10x50" };
  }
  console.log("[m9-13-performance] 运行 M9-05 10×50 事件桥报告");
  const result = spawnSync(
    "cargo",
    [
      "test",
      "-p",
      "aether-app",
      "--test",
      "m9_05_event_bridge",
      "ten_sessions_fifty_deltas_per_second_keep_all_control_events",
      "-j",
      "1",
      "--",
      "--test-threads=1",
      "--nocapture",
    ],
    {
      cwd: repoRoot,
      encoding: "utf8",
      env: { ...process.env, CARGO_BUILD_JOBS: "1" },
      timeout: 15 * 60 * 1000,
    },
  );
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  const matches = [...output.matchAll(/M9_05_10X50_REPORT (\{.*\})/g)];
  if (result.status !== 0) {
    return {
      passed: false,
      exit: result.status,
      output_tail: output.split(/\r?\n/).slice(-40).join("\n"),
    };
  }
  if (matches.length === 0) {
    return { passed: false, reason: "未找到 M9_05_10X50_REPORT", output_tail: output.slice(-2000) };
  }
  const report = JSON.parse(matches[matches.length - 1][1]);
  return {
    passed:
      report.sessions === 10 &&
      report.deltas_per_second === 50 &&
      report.control_lost === 0 &&
      report.p95_ms < thresholds.events.p95_latency_max_ms,
    report,
  };
}

async function measureApp() {
  if (!existsSync(daemonPath)) {
    fail(`缺少 aetherd：${daemonPath}（先 pnpm prepare:m9-12:e2e）`);
  }
  if (!existsSync(path.join(bundleDir, "runtimes.json"))) {
    fail(`缺少 Mock runtime-bundle：${bundleDir}（先 pnpm prepare:m9-12:e2e）`);
  }
  if (!existsSync(path.join(appRoot, "dist", "index.html"))) {
    fail("缺少 renderer 构建产物（先 pnpm build:desktop）");
  }

  const base = path.join(repoRoot, "dist-out", "m9-13-performance");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, "run-"));
  const dataDir = path.join(root, "data");
  const userData = path.join(root, "user");
  const downloads = path.join(root, "downloads");
  const tmp = path.join(root, "tmp");
  for (const dir of [dataDir, userData, downloads, tmp]) mkdirSync(dir, { recursive: true });

  let app = null;
  try {
    app = await _electron.launch({
      executablePath: electronPath,
      args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
      cwd: repoRoot,
      env: {
        ...process.env,
        AETHER_E2E: "1",
        AETHER_E2E_DOWNLOADS_DIR: downloads,
        AETHER_USER_DATA_DIR: userData,
        AETHER_DATA_DIR: dataDir,
        AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
        AETHER_RUNTIME_REGISTRY_DIR: bundleDir,
        AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
        AETHER_SECRETS_PASSPHRASE: "m9-13-performance-passphrase",
        AETHERD_BIN: daemonPath,
        USERPROFILE: root,
        HOME: root,
        TEMP: tmp,
        TMP: tmp,
      },
      timeout: 40_000,
    });
    const page = await waitFor(
      async () => {
        const pages = app.windows().filter((item) => !item.isClosed());
        return pages.find((item) => item.url() === APP_URL) ?? null;
      },
      30_000,
      "主窗口",
    );
    await page.waitForSelector("[data-testid='workbench']", { timeout: 20_000 });
    const state = await waitFor(
      async () => {
        const value = await app.evaluate(
          () => globalThis.__aetherShell?.snapshot?.() ?? null,
        );
        return value?.ready ? value : null;
      },
      60_000,
      "daemon ready",
    );
    const rpc = async (method, params) =>
      page.evaluate(
        ([name, value]) => window.aether.rpc(name, value),
        [method, params],
      );
    await waitFor(
      async () => {
        const envelope = await rpc("health", {});
        return envelope?.ok === true ? envelope : null;
      },
      30_000,
      "health",
    );
    const readyState = await app.evaluate(
      () => globalThis.__aetherShell?.snapshot?.() ?? null,
    );
    await sleep(2_000);
    const idleRss = measureProcessTree(app.process()?.pid);

    const created = await rpc("session_create", {
      runtime_id: "mock",
      title: "M9-13 performance session",
    });
    if (!created?.ok) {
      throw new Error(`session_create 失败：${JSON.stringify(created?.error ?? created)}`);
    }
    const afterCreate = await waitFor(
      async () => {
        const value = await app.evaluate(
          () => globalThis.__aetherShell?.snapshot?.() ?? null,
        );
        return value?.sessionCreateMs !== null ? value : null;
      },
      30_000,
      "sessionCreateMs",
    );
    for (let index = 2; index <= 10; index += 1) {
      const response = await rpc("session_create", {
        runtime_id: "mock",
        title: `M9-13 performance ${index}`,
      });
      if (!response?.ok) {
        throw new Error(`第 ${index} 个会话创建失败：${JSON.stringify(response?.error)}`);
      }
    }
    const sessions = await rpc("session_list", {});
    const sessionCount = Array.isArray(sessions?.result) ? sessions.result.length : 0;
    const tenSessionsRss = measureProcessTree(app.process()?.pid);

    const metrics = {
      started_at: readyState.startedAt,
      window_visible_ms: (readyState.windowReadyAt ?? readyState.startedAt) - readyState.startedAt,
      initialize_ms: readyState.initializeMs,
      ui_ready_ms:
        readyState.uiReadyAt === null
          ? null
          : readyState.uiReadyAt - readyState.startedAt,
      session_create_ms: afterCreate.sessionCreateMs,
      idle_rss_bytes: idleRss.total_bytes,
      idle_rss_mib: idleRss.total_mib,
      ten_sessions_rss_bytes: tenSessionsRss.total_bytes,
      ten_sessions_rss_mib: tenSessionsRss.total_mib,
      sessions: sessionCount,
      daemon_pid: readyState.daemonPid ?? null,
    };
    return { metrics, dataDir, root, app };
  } catch (error) {
    if (app) killTree(app.process()?.pid);
    throw error;
  }
}

async function main() {
  const startedAt = new Date().toISOString();
  const appResult = await measureApp();
  const { metrics, dataDir, root, app } = appResult;
  const electronPid = app.process()?.pid;

  // 关闭并记录 WAL（drain 先于 checkpoint；WAL 应已 TRUNCATE/回收）。
  let closed = false;
  try {
    await app.close();
    closed = true;
  } catch (error) {
    console.warn(`[m9-13-performance] app.close 失败（整树回收）：${error.message}`);
  }
  if (!closed) killTree(electronPid);
  await sleep(1_500);
  if (electronPid && process.platform === "win32") {
    // 确认无残留；残留则整树回收，避免影响下一次采样。
    const still = spawnSync("tasklist", ["/FI", `PID eq ${electronPid}`], { encoding: "utf8" });
    if ((still.stdout ?? "").includes(String(electronPid))) killTree(electronPid);
  }
  const walPath = path.join(dataDir, "aether.db-wal");
  const walBytes = existsSync(walPath) ? statSync(walPath).size : 0;

  const installer = latestInstaller();
  const tenByFifty = run10x50();
  const smoke = runSmokeInstaller();
  const packaged = smoke.value?.metrics ?? null;
  const checks = [];
  const record = (name, ok, detail) => {
    checks.push({ name, ok, detail });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
  };

  // 启动/RSS 硬阈值优先使用**安装产物**（M9-11 test MSI 冒烟）的口径；
  // 开发态 Playwright 采样（metrics）作为补充证据（debug 启动含调试器开销）。
  const startup = packaged?.first_launch ?? metrics;
  const windowMs = startup.window_visible_ms;
  const uiReadyMs = startup.ui_ready_ms;
  const idleRssBytes = startup.idle_rss_bytes;
  const idleRssMib = startup.idle_rss_mib;
  const tenSessionsBytes = packaged?.ten_sessions_upper_bound_bytes ?? metrics.ten_sessions_rss_bytes;
  const tenSessionsMib = packaged?.ten_sessions_upper_bound_mib ?? metrics.ten_sessions_rss_mib;
  const startupSource = packaged?.first_launch ? "安装产物" : "开发态";

  record(
    `窗口可见 ≤1.5s（${startupSource}）`,
    Number(windowMs) > 0 && Number(windowMs) <= thresholds.startup.window_shown_max_ms,
    `${windowMs}ms ≤ ${thresholds.startup.window_shown_max_ms}ms`,
  );
  record(
    "initialize 握手目标 ≤2s（硬超时 10s）",
    metrics.initialize_ms > 0 && metrics.initialize_ms <= thresholds.startup.handshake_target_ms,
    `${metrics.initialize_ms}ms ≤ ${thresholds.startup.handshake_target_ms}ms（硬超时 ${thresholds.startup.handshake_timeout_ms}ms）`,
  );
  record(
    `UI ready ≤8s（${startupSource}）`,
    Number(uiReadyMs) > 0 && Number(uiReadyMs) <= thresholds.startup.ui_ready_max_ms,
    `${uiReadyMs}ms ≤ ${thresholds.startup.ui_ready_max_ms}ms`,
  );
  record(
    "会话创建 <2s",
    metrics.session_create_ms !== null &&
      metrics.session_create_ms < thresholds.startup.session_create_max_ms,
    `${metrics.session_create_ms}ms < ${thresholds.startup.session_create_max_ms}ms`,
  );
  record(
    `idle RSS ≤600MiB（${startupSource}）`,
    Number(idleRssBytes) > 0 &&
      Number(idleRssBytes) <= thresholds.runtime.idle_rss_max_bytes,
    `${idleRssMib} MiB ≤ ${(thresholds.runtime.idle_rss_max_bytes / 1048576).toFixed(0)} MiB`,
  );
  record(
    `10 会话 RSS ≤1.2GiB（${packaged?.ten_sessions_upper_bound_bytes ? "安装产物保守上界" : "开发态"}）`,
    Number(tenSessionsBytes) > 0 &&
      Number(tenSessionsBytes) <= thresholds.runtime.ten_sessions_max_bytes,
    `${tenSessionsMib} MiB ≤ ${(thresholds.runtime.ten_sessions_max_bytes / 1048576).toFixed(0)} MiB`,
  );
  record("10 会话可创建/列出", metrics.sessions >= 10, `sessions=${metrics.sessions}`);
  const installerBytes = packaged?.installer?.bytes ?? installer?.bytes;
  if (installerBytes) {
    const withinRatio = Number(installerBytes) <= thresholds.installer.max_bytes;
    const withinCap = Number(installerBytes) <= thresholds.installer.absolute_cap_bytes;
    record(
      "安装包体积 ≤1.5×HEAD 且 ≤450MiB",
      withinRatio && withinCap,
      `${installerBytes} B ≤ ${thresholds.installer.max_bytes} B（cap ${thresholds.installer.absolute_cap_bytes} B）`,
    );
  } else {
    record("安装包体积", false, "dist-out 无 MSI/DMG（先 pnpm package:desktop）");
  }
  record("安装产物冒烟通过（M9-11 test MSI）", smoke.skipped === true || smoke.passed === true, smoke.skipped ? smoke.reason : `exit=${smoke.exit ?? 0}`);
  if (!tenByFifty.skipped) {
    record(
      "10×50 控制事件 0 丢失",
      tenByFifty.passed === true && tenByFifty.report?.control_lost === 0,
      tenByFifty.report
        ? `control=${tenByFifty.report.control_received}/${tenByFifty.report.control_expected} lost=${tenByFifty.report.control_lost}`
        : tenByFifty.reason ?? tenByFifty.output_tail ?? "",
    );
    record(
      "10×50 P95 <150ms",
      tenByFifty.passed === true &&
        tenByFifty.report?.p95_ms < thresholds.events.p95_latency_max_ms,
      tenByFifty.report
        ? `${tenByFifty.report.p95_ms}ms < ${thresholds.events.p95_latency_max_ms}ms`
        : "无报告",
    );
  } else {
    checks.push({ name: "10×50（跳过）", ok: true, skipped: true, detail: tenByFifty.reason });
    console.log(`SKIP  10×50（${tenByFifty.reason}）`);
  }
  record("关闭后 WAL 已 checkpoint（≤4KiB）", walBytes <= 4096, `${walBytes} B`);

  const passed = checks.every((item) => item.ok);
  const evidence = {
    schema: "aether.m9-13.performance.v1",
    task: "M9-13",
    started_at: startedAt,
    finished_at: new Date().toISOString(),
    platform: {
      os: process.platform,
      arch: process.arch,
      node: process.version,
    },
    thresholds,
    metrics: {
      ...metrics,
      packaged: packaged
        ? {
            source: smoke.file ? path.relative(repoRoot, smoke.file).split(path.sep).join("/") : null,
            first_launch: packaged.first_launch ?? null,
            ten_sessions_upper_bound_bytes: packaged.ten_sessions_upper_bound_bytes ?? null,
            ten_sessions_upper_bound_mib: packaged.ten_sessions_upper_bound_mib ?? null,
            installer: packaged.installer ?? null,
          }
        : null,
      wal_bytes_after_shutdown: walBytes,
      installer: installer
        ? {
            path: path.relative(repoRoot, installer.path).split(path.sep).join("/"),
            bytes: installer.bytes,
            max_bytes: thresholds.installer.max_bytes,
            absolute_cap_bytes: thresholds.installer.absolute_cap_bytes,
          }
        : null,
      ten_by_fifty: tenByFifty,
    },
    checks,
    passed,
  };
  writeFileSync(outPath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
  console.log(`[m9-13-performance] 证据：${path.relative(repoRoot, outPath)}`);
  try {
    rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-13-performance] 清理临时目录失败（忽略）：${error.message}`);
  }
  if (!passed) {
    console.error(`[m9-13-performance] FAIL ${checks.filter((item) => !item.ok).length} 项`);
    process.exit(1);
  }
  console.log("[m9-13-performance] PASS");
  console.log("AETHER_M9_13_PERFORMANCE PASS");
}

await main();
