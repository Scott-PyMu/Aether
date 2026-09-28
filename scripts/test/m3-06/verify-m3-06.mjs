/**
 * M3-06 验证入口：崩溃恢复体验（设计 D2/D4/D5/D8、ADR-004/ADR-005）。
 *
 * 覆盖 DoD：
 *   1) T4：kill -9 ×20（run 中）→「已确认」零丢失（用户消息=send 返回 runId；
 *      助手消息=收到 completed 且 journal 提交）；未确认不误显示完成 + 重启收口
 *      —— `m3_06_t4`（宿主子进程强杀 ×20 + 校验子进程；证据行归档）；
 *   2) `run_retry` IPC：仅终态 run；参数校验；重放按 Mode R/N（fixture `--mode session`：
 *      首次 `resumed=false`（Mode N）、核心重启后 `resumed=true`（Mode R））；
 *      新 run + 旧 run 保留审计 —— `m3_06_retry`（生命周期）+ `m3_06_run_retry`（IPC）；
 *   3) `persist_degraded` + 只读横幅 UI（发送入口禁用、在途 run 已中断提示）+
 *      `app_restart` 恢复引导（注入 + E2E）—— 前端测试（`m3_06_recovery.test.tsx`）
 *      + 真实 WebView E2E（`e2e-degraded-recovery.mjs`，Windows；默认执行）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；夹具由本脚本构建并注入
 * `AETHER_FIXTURE_BIN`（`AETHER_REQUIRE_FIXTURE=1` 强制）。无需 Bun。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const args = process.argv.slice(2);
const skipE2e = args.includes("--skip-e2e");

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-06", `evidence-${stamp}`);

/** 同步执行并捕获输出（透传打印；供证据行解析）。 */
function capture(cargoArgs, env) {
  console.log(`\n$ ${[cargo, ...cargoArgs].join(" ")}${env ? "   # 附加环境变量" : ""}`);
  const result = spawnSync(cargo, cargoArgs, {
    cwd: repoRoot,
    env: { ...process.env, ...(env ?? {}) },
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
  });
  const out = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  process.stdout.write(out);
  console.log(`[exec] exit=${result.status ?? 1}`);
  return { status: result.status ?? 1, out };
}

/** 解析机器可读证据行（`AETHER_M3_06_T4_*`）。 */
function parseEvidence(out, label) {
  const line = out
    .split(/\r?\n/)
    .map((value) => value.trim())
    .find((value) => value.includes(label));
  if (!line) return null;
  const index = line.indexOf(label);
  try {
    return JSON.parse(line.slice(index + label.length).trim());
  } catch {
    return null;
  }
}

// ===== 0. 构建会话夹具（Mode R/N 重放宿主；M3-06 复用 M1-10 夹具二进制）=====

record(
  "构建 aether-adapter-fixture（cargo build -p aether-adapters --bin aether-adapter-fixture）",
  spawnSync(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"], {
    cwd: repoRoot,
    stdio: "inherit",
  }).status === 0
    ? 0
    : 1,
);
record(`夹具产物存在：${path.relative(repoRoot, fixture)}`, existsSync(fixture) ? 0 : 1);

const fixtureEnv = {
  AETHER_FIXTURE_BIN: fixture,
  AETHER_REQUIRE_FIXTURE: "1",
};

// ===== 1. DoD2：生命周期 `run_retry` + 重启状态重建（aether-control）=====

const retry = capture(["test", "-p", "aether-control", "--test", "m3_06_retry", "--", "--nocapture"]);
record(
  "m3_06_retry（仅终态可重试/复用输入消息/旧 run 审计/降级拒绝/重启状态重建）",
  retry.status,
);

// ===== 2. DoD2：IPC `run_retry` + Mode R/N（aether-tauri + 会话夹具）=====

const ipc = capture(
  ["test", "-p", "aether-tauri", "--test", "m3_06_run_retry", "--", "--nocapture"],
  fixtureEnv,
);
record(
  "m3_06_run_retry（run_retry JSON 契约；Mode N 首次创建 + Mode R 核心重启后 native_id 恢复）",
  ipc.status,
);

// ===== 3. DoD1：T4 kill -9 ×20（宿主子进程 + 校验子进程）=====

const t4 = capture(["test", "-p", "aether-tauri", "--test", "m3_06_t4", "--", "--nocapture"]);
record("m3_06_t4（kill -9 ×20；已确认零丢失 + 未确认不误显示完成 + 重启收口）", t4.status);
const t4Verify = parseEvidence(t4.out, "AETHER_M3_06_T4_VERIFY");
record(
  "T4 证据：已确认用户/助手消息零丢失；未确认 run 非 succeeded 且被收口 failed(run_interrupted)",
  t4Verify !== null &&
    t4Verify.user_confirmed === 40 &&
    t4Verify.assistant_confirmed === 20 &&
    t4Verify.inflight === 20 &&
    Array.isArray(t4Verify.loss) &&
    t4Verify.loss.length === 0 &&
    Array.isArray(t4Verify.false_completions) &&
    t4Verify.false_completions.length === 0 &&
    Array.isArray(t4Verify.unreconciled) &&
    t4Verify.unreconciled.length === 0
    ? 0
    : 1,
);

// ===== 4. DoD3：前端降级 UX（发送禁用/中断提示/重试按钮/横幅恢复入口）=====

record(
  "pnpm --filter @aether/desktop test（M3-06 恢复体验 + 既有回归）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "test"], { cwd: repoRoot }),
);

// ===== 5. DoD3：真实 WebView E2E（降级横幅 → app_restart → 重启后自检）=====

if (skipE2e) {
  checks.push({
    name: "m3-06 E2E（降级恢复引导；真实 WebView，Windows）",
    skip: true,
    reason: "显式 --skip-e2e（本地无 WebView2/前端构建时）；CI security-baseline 执行",
  });
} else if (process.platform !== "win32") {
  checks.push({
    name: "m3-06 E2E（降级恢复引导；真实 WebView）",
    skip: true,
    reason: "仅 Windows WebView2 宿主",
  });
} else {
  record(
    "m3-06 E2E（降级横幅 → app_restart → 重启后启动自检；真实 WebView2）",
    run(process.execPath, [path.join(repoRoot, "scripts", "test", "m3-06", "e2e-degraded-recovery.mjs")], {
      cwd: repoRoot,
    }),
  );
}

// ===== 6. 静态检查：接线与契约在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const executor = read("crates/aether-tauri/src/adapter_executor.rs");
  const probe = read("crates/aether-tauri/src/m3_06_probe.rs");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const healthMonitor = read("apps/desktop/src/HealthMonitor.tsx");
  const healthBus = read("apps/desktop/src/healthBus.ts");
  const required = [
    [lifecycle, "pub async fn retry_run", "run_retry 生命周期实现"],
    [lifecycle, "run_is_retryable", "终态准入（仅 failed/timeout/cancelled）"],
    [lifecycle, "pub async fn reconcile_interrupted_runs", "重启状态重建"],
    [lifecycle, "RUN_INTERRUPTED_CODE", "run_interrupted 错误码"],
    [sessionBackend, "fn run_retry", "run_retry IPC 接线"],
    [commands, "fn app_restart", "app_restart 命令层实现"],
    [commands, "request_restart", "AppHandle::request_restart（复用 D2 关闭序列）"],
    [lib, "reconcile_interrupted_runs", "生产启动路径接线重启状态重建"],
    [executor, "request.run_id.as_str()", "适配器幂等键按 run id（重放不被吞）"],
    [probe, "AETHER_E2E_M3_06_PROBE", "M3-06 E2E 探针在案"],
    [healthBus, "publishHealthState", "健康共享总线（单轮询源）"],
    [healthMonitor, "storage-degraded-restart", "降级横幅 app_restart 入口"],
    [workbench, "composer-degraded-hint", "降级期发送入口禁用提示"],
    [workbench, "run-retry", "失败/中断 run 重试入口"],
    [workbench, "已中断（存储降级）", "降级中断提示"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // 禁止热恢复（D4/ADR-004）：由 E2E 探针在真实 DOM 断言
  // （`hot_recovery=false`；不提供「一键恢复/自动恢复」入口），此处不重复静态文本匹配。
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：run_retry/重启重建/app_restart/降级 UX 接线在案", problems.length === 0 ? 0 : 1);
}

// ===== 7. 证据归档（T4 校验 JSON + 汇总）=====

{
  mkdirSync(evidenceDir, { recursive: true });
  if (t4Verify !== null) {
    writeFileSync(
      path.join(evidenceDir, "t4-kill9-verify.json"),
      `${JSON.stringify(t4Verify, null, 2)}\n`,
      "utf8",
    );
  }
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(
      {
        task: "M3-06",
        stamp,
        t4: t4Verify,
        thresholds: { iterations: 20, confirmed_user: 40, confirmed_assistant: 20 },
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
  console.log(`[m3-06] 证据目录：${evidenceDir}`);
  record(
    "证据归档：T4 校验 JSON 落盘（供 Gate 3 逐条出示）",
    existsSync(path.join(evidenceDir, "summary.json")) && t4Verify !== null ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-06", checks));
