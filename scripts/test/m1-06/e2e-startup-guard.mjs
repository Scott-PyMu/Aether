/**
 * M1-06 E2E：数据目录拒绝启动流（T13）+ 单实例锁（T12）。
 *
 * 覆盖 DoD3 / DoD5：
 *   1. 模拟 OneDrive 环境变量命中 → 真实 WebView 中只渲染「迁移/退出」，
 *      主界面（app-version）不可达，业务命令返回 startup_blocked；
 *   2. 二次启动 → 第二进程退出、首实例收到聚焦回调；
 *   3. 点击「迁移到本地目录」→ 复制 → sha256 校验 → 原子替换 → 指针锁定新目录；
 *   4. 重启（不带模拟 OneDrive / 数据目录环境变量）→ 以指针目录启动，主界面可达。
 *
 * 用法：node scripts/test/m1-06/e2e-startup-guard.mjs [--skip-frontend-build] [--skip-rust-build]
 */
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");

const PHASE_LINE = "AETHER_M1_06_PHASE";
const REPORT_LINE = "AETHER_M1_06_REPORT";
const FOCUS_LINE = "AETHER_M1_06_FOCUS";
const EXIT_LINE = "AETHER_M1_06_EXIT";

const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
const binary = path.join(repoRoot, "target", "debug", binaryName);
const frontendDist = path.join(repoRoot, "apps", "desktop", "dist");

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const workDir = path.join(repoRoot, "scripts", "test", ".tmp", "m1-06", stamp);
const syncRoot = path.join(workDir, "OneDrive-sim");
const sourceDir = path.join(syncRoot, "Aether");
const targetDir = path.join(workDir, "local-target");
const configDir = path.join(workDir, "config");
const pointerFile = path.join(configDir, "data-location.json");
const triggerFile = path.join(workDir, "migrate.trigger");

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const sha256 = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");

function launch(extraEnv, label) {
  const child = spawn(binary, [], {
    env: { ...env, ...extraEnv },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const state = { lines: [], stderr: [], exited: false, code: null };
  child.stdout.on("data", (chunk) => {
    for (const line of String(chunk).split(/\r?\n/)) {
      if (!line.trim()) continue;
      state.lines.push(line);
      console.log(`[${label}] ${line}`);
    }
  });
  child.stderr.on("data", (chunk) => {
    const text = String(chunk);
    state.stderr.push(text.trimEnd());
    console.error(`[${label}:stderr] ${text.trimEnd()}`);
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
  throw new Error(
    [
      `等待超时：${description}（已收到 ${state.lines.length} 行 stdout，进程已退出=${state.exited}，退出码=${state.code}）`,
      "--- stdout ---",
      ...state.lines,
      "--- stderr ---",
      ...state.stderr,
    ].join("\n"),
  );
}

async function exitCode(processRef, timeoutMs) {
  const result = await Promise.race([
    processRef.exit,
    sleep(timeoutMs).then(() => "timeout"),
  ]);
  if (result === "timeout") {
    processRef.child.kill();
    return "timeout";
  }
  return result;
}

const processes = [];
function start(extraEnv, label) {
  const ref = launch(extraEnv, label);
  processes.push(ref);
  return ref;
}

try {
  // -------------------------------------------------------------------------
  // 0. 构建前端与壳层（沿用 M1-08 E2E 口径：custom-protocol 加载内嵌 dist）
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
  // 1. 夹具：模拟 OneDrive 数据目录 / 本地迁移目标 / 指针与触发文件路径
  // -------------------------------------------------------------------------
  mkdirSync(path.join(sourceDir, "backups"), { recursive: true });
  mkdirSync(targetDir, { recursive: true });
  mkdirSync(configDir, { recursive: true });
  writeFileSync(path.join(sourceDir, "aether.db"), "aether-db-bytes");
  writeFileSync(path.join(sourceDir, "aether.db-wal"), "wal-bytes");
  writeFileSync(path.join(sourceDir, "backups", "b1.db"), "backup-bytes");
  console.log(`[e2e] 工作目录：${workDir}`);

  const blockedEnv = {
    ...env,
    OneDrive: syncRoot,
    AETHER_DATA_DIR: sourceDir,
    AETHER_DATA_LOCATION_FILE: pointerFile,
    AETHER_E2E_STARTUP_PROBE: "1",
    AETHER_E2E_MIGRATE_TARGET: targetDir,
    AETHER_E2E_TRIGGER_FILE: triggerFile,
  };

  // -------------------------------------------------------------------------
  // 2. 阶段一：拒绝启动（T13）
  // -------------------------------------------------------------------------
  console.log(`\n$ ${binary}   # AETHER_E2E_STARTUP_PROBE=1 + OneDrive=${syncRoot}`);
  const first = start(blockedEnv, "app1");
  const phaseLine = await waitFor(
    first.state,
    (line) => line.startsWith(PHASE_LINE) && line.includes('"blocked_sync_dir"'),
    90000,
    "启动门阻塞快照",
  );
  const phase = JSON.parse(phaseLine.slice(PHASE_LINE.length).trim());
  record(
    "启动自检命中同步盘并拒绝启动（blocked_sync_dir）",
    phase.phase === "blocked_sync_dir" && phase.detection?.verdict === "reject",
    JSON.stringify(phase.detection?.reasons ?? []),
  );

  const blockedLine = await waitFor(
    first.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"blocked"'),
    90000,
    "阻塞态 DOM 回报",
  );
  const blocked = JSON.parse(blockedLine.slice(REPORT_LINE.length).trim());
  const buttons = blocked.buttons ?? [];
  record(
    "拒绝启动界面：迁移/退出可达、主界面不可达",
    blocked.gate === true &&
      blocked.main === false &&
      buttons.includes("startup-migrate") &&
      buttons.includes("startup-exit"),
    `buttons=${JSON.stringify(buttons)}`,
  );
  record(
    "命令层阻断：阻塞态业务命令返回 startup_blocked",
    blocked.blocked === "startup_blocked",
    String(blocked.blocked),
  );

  // -------------------------------------------------------------------------
  // 3. 阶段二：单实例（T12）
  // -------------------------------------------------------------------------
  console.log(`\n$ ${binary}   # 第二实例（同环境）`);
  const second = start(blockedEnv, "app2");
  const secondExit = await exitCode(second, 30000);
  record("T12 第二实例退出", secondExit === 0, `exit=${secondExit}`);
  const secondReports = second.state.lines.filter((line) => line.startsWith(REPORT_LINE));
  record(
    "T12 第二实例未进入启动自检（无主界面/无写连接前置）",
    secondReports.length === 0,
    `第二进程 report 行数=${secondReports.length}`,
  );

  await waitFor(
    first.state,
    (line) => line.startsWith(FOCUS_LINE),
    60000,
    "首实例聚焦回调",
  );
  record("T12 首实例收到第二实例转发并聚焦已有窗口", true);

  // -------------------------------------------------------------------------
  // 4. 阶段三：点击迁移（复制 → 校验 → 原子替换 → 锁定新目录）
  // -------------------------------------------------------------------------
  console.log(`\n$ trigger ${triggerFile}`);
  writeFileSync(triggerFile, "migrate");
  await waitFor(
    first.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"clicked"'),
    60000,
    "点击「迁移到本地目录」",
  );
  await waitFor(
    first.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"ready"'),
    180000,
    "迁移后主界面可达",
  );
  const firstExit = await exitCode(first, 30000);
  record("迁移后应用正常退出（探针）", firstExit === 0, `exit=${firstExit}`);
  record(
    "迁移目标包含主库且与源 sha256 一致",
    existsSync(path.join(targetDir, "aether.db")) &&
      sha256(path.join(targetDir, "aether.db")) === sha256(path.join(sourceDir, "aether.db")),
  );
  record(
    "迁移目标包含备份子目录（目录结构保持）",
    existsSync(path.join(targetDir, "backups", "b1.db")) &&
      sha256(path.join(targetDir, "backups", "b1.db")) ===
        sha256(path.join(sourceDir, "backups", "b1.db")),
  );
  record("迁移后源数据目录保留（回滚副本）", existsSync(path.join(sourceDir, "aether.db")));
  const pointer = JSON.parse(readFileSync(pointerFile, "utf8"));
  record(
    "迁移成功后锁定新目录（指针原子替换）",
    pointer.data_dir === targetDir,
    pointer.data_dir,
  );

  // -------------------------------------------------------------------------
  // 5. 阶段四：重启锁定（不带 OneDrive / AETHER_DATA_DIR，仅指针）
  // -------------------------------------------------------------------------
  console.log(`\n$ ${binary}   # 指针锁定重启`);
  await sleep(800); // 留出前一实例单实例锁的释放窗口
  const lockedEnv = { ...env, AETHER_E2E_STARTUP_PROBE: "1", AETHER_DATA_LOCATION_FILE: pointerFile };
  delete lockedEnv.OneDrive;
  delete lockedEnv.OneDriveConsumer;
  delete lockedEnv.OneDriveCommercial;
  delete lockedEnv.AETHER_DATA_DIR;
  const third = start(lockedEnv, "app3");
  const lockedLine = await waitFor(
    third.state,
    (line) => line.startsWith(PHASE_LINE) && line.includes('"ready"'),
    120000,
    "锁定重启就绪快照",
  );
  const locked = JSON.parse(lockedLine.slice(PHASE_LINE.length).trim());
  record(
    "重启后以指针目录启动（锁定新目录）",
    locked.phase === "ready" &&
      locked.data_dir_source === "pointer" &&
      locked.data_dir === targetDir,
    `${locked.data_dir_source} ${locked.data_dir}`,
  );
  await waitFor(
    third.state,
    (line) => line.startsWith(REPORT_LINE) && line.includes('"stage":"ready"'),
    120000,
    "锁定重启主界面可达",
  );
  const thirdExit = await exitCode(third, 30000);
  record("锁定新目录后主界面可达并正常退出", thirdExit === 0, `exit=${thirdExit}`);
} catch (error) {
  record("E2E 执行", false, String(error && error.message ? error.message : error));
} finally {
  for (const ref of processes) {
    if (!ref.state.exited) ref.child.kill();
  }
}

// 不用 process.exit：避免管道输出未 flush 导致 CI 日志截断（诊断需要完整 FAIL 行）。
process.exitCode = summarize("m1-06-startup-guard-e2e", checks);
