/**
 * M2-08 验证入口：孤儿清理与退出可靠性（设计 D5 / 评审 #5；附录 D T5b/T11）。
 *
 * 覆盖 DoD：
 *   1) 启动清理三条件（集成，复用 M1-10 夹具）——子进程扮演核心并经真实监督器预热
 *      适配器后被强杀（Windows TerminateProcess / Unix SIGKILL），孤儿存活；重启走
 *      应用启动路径（`boot_supervisor` + `run_supervisor_startup`）：
 *      token 命中孤儿整树回收；PID 复用（启动时间不符）与令牌不符诱饵 0 误杀；
 *   2) T11：适配器无响应下退出 ≤10s、进程快照无残留；Windows 机制序列断言
 *      `TerminateJobObject` 优先（`taskkill /T /F` 兜底未触发）；存储五步顺序 +
 *      `-wal` 归零；
 *   3) T5b：deaf 夹具（进入不响应模式，不使用 SIGSTOP）→ 严格 D5 心跳 10s/5s/连续
 *      3 次失败 ≤45s 触发重启、120s 内 Ready、旧进程无残留。
 *
 * 另含静态检查（Tauri ExitRequested 接线 / 启动序列接线 / D5 机制词典）与证据归档
 * （`scripts/test/.tmp/m2-08/evidence-<stamp>/`）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；夹具由本脚本构建并注入
 * `AETHER_FIXTURE_BIN`（`AETHER_REQUIRE_FIXTURE=1` 强制）。无需 Bun。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, ok) => checks.push({ name, exit: ok ? 0 : 1, expect: 0 });

const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m2-08", `evidence-${stamp}`);

/** 同步执行并捕获输出（透传打印；供证据行解析）。 */
function capture(args, env) {
  console.log(`\n$ ${[cargo, ...args].join(" ")}${env ? "   # 附加环境变量：夹具路径" : ""}`);
  const result = spawnSync(cargo, args, {
    cwd: repoRoot,
    env: { ...process.env, ...(env ?? {}) },
    encoding: "utf8",
    maxBuffer: 128 * 1024 * 1024,
  });
  const out = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  process.stdout.write(out);
  console.log(`[exec] exit=${result.status ?? 1}`);
  return { status: result.status ?? 1, out };
}

/** 解析机器可读证据行（`AETHER_M2_08_*`）。 */
function parseEvidence(out, label) {
  const line = out
    .split(/\r?\n/)
    .map((value) => value.trim())
    .find((value) => value.startsWith(label));
  if (!line) return null;
  try {
    return JSON.parse(line.slice(label.length).trim());
  } catch {
    return null;
  }
}

// ===== 0. 构建故障注入夹具（M1-10 复用；M2-08 孤儿/无响应场景宿主）=====

record(
  "构建 aether-adapter-fixture（cargo build -p aether-adapters --bin aether-adapter-fixture）",
  spawnSync(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"], {
    cwd: repoRoot,
    stdio: "inherit",
  }).status === 0,
);
record(`夹具产物存在：${path.relative(repoRoot, fixture)}`, existsSync(fixture));

const fixtureEnv = {
  AETHER_FIXTURE_BIN: fixture,
  AETHER_REQUIRE_FIXTURE: "1",
};

// ===== 1. DoD1：强杀核心 → 重启启动清理（token 命中回收；PID 复用 0 误杀）=====

const orphans = capture(
  ["test", "-p", "aether-tauri", "--test", "m2_08_orphans", "--", "--nocapture", "--test-threads=1"],
  fixtureEnv,
);
record("DoD1 m2_08_orphans（强杀核心 → 重启清理；真实进程集成）", orphans.status === 0);
const dod1 = parseEvidence(orphans.out, "AETHER_M2_08_DOD1 ");
record(
  "DoD1 证据：token 命中孤儿 =1 回收；PID 复用/令牌不符诱饵 killed=0 且存活",
  dod1 !== null &&
    dod1.reclaimed === 1 &&
    dod1.killed_decoys === 0 &&
    dod1.reuse_decoy_alive === true &&
    dod1.token_decoy_alive === true,
);

// ===== 2. DoD2 T11：无响应退出 ≤10s、无残留、Job Object 优先 =====

const exit = capture(
  ["test", "-p", "aether-tauri", "--test", "m2_08_exit", "--", "--nocapture"],
  fixtureEnv,
);
record("DoD2 m2_08_exit（T11 退出编排：管线停机 + 适配器终止段 + 存储五步）", exit.status === 0);
const t11 = parseEvidence(exit.out, "AETHER_M2_08_T11 ");
record(
  "T11 证据：elapsed ≤10s；适配器 exited 且无残留；存储五步顺序与 -wal 归零",
  t11 !== null &&
    t11.elapsed_ms <= 10_000 &&
    t11.within_budget === true &&
    t11.deadline_expired === false &&
    t11.adapter?.exited === true &&
    t11.adapter_residual === false &&
    t11.storage?.d2_order === true &&
    t11.storage?.wal_bytes_after === 0,
);
if (process.platform === "win32") {
  record(
    "T11 Windows 机制断言：TerminateJobObject 优先（无 taskkill /T /F 兜底）",
    t11 !== null &&
      Array.isArray(t11.adapter?.mechanisms) &&
      t11.adapter.mechanisms.includes("terminate_job_object") &&
      !t11.adapter.mechanisms.includes("taskkill_tree_force"),
  );
}

// ===== 3. DoD3 T5b：不响应模式 → 心跳连续 3 次失败 ≤45s → 120s 内 Ready =====

const t5b = capture(["test", "-p", "aether-adapters", "--test", "m2_08_t5b", "--", "--nocapture"]);
record("DoD3 m2_08_t5b（严格 D5 心跳 10s/5s/连续 3 次；不压缩时间参数）", t5b.status === 0);
const t5bEvidence = parseEvidence(t5b.out, "AETHER_M2_08_T5B ");
record(
  "T5b 证据：触发 ≤45s、Ready ≤120s、reason=heartbeat_failed、旧进程无残留",
  t5bEvidence !== null &&
    t5bEvidence.trigger_ms <= 45_000 &&
    t5bEvidence.ready_ms <= 120_000 &&
    t5bEvidence.first_pid_residual === false &&
    Array.isArray(t5bEvidence.transitions) &&
    t5bEvidence.transitions.includes("ready→degraded") &&
    t5bEvidence.transitions.includes("degraded→starting") &&
    t5bEvidence.reasons?.includes("heartbeat_failed") === true,
);

// ===== 4. 回归：M1-10 监督器用例（夹具语义变更后必须全绿）=====

const regression = capture(["test", "-p", "aether-adapters", "--lib", "supervisor"]);
record("回归 m2-08 触及面：aether-adapters supervisor 单测", regression.status === 0);

// ===== 5. 静态检查：应用启动/退出接线与 D5 机制词典在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

  const lib = read("crates/aether-tauri/src/lib.rs");
  for (const [needle, label] of [
    ["run_supervisor_startup", "启动序列尾段（孤儿清理 + 预热 + 监控）接线"],
    ["install_shutdown", "退出编排注入"],
    ["shutdown::on_exit_requested", "ExitRequested 回调接线"],
    ["RunEvent::ExitRequested", "Tauri 退出事件"],
    ["boot_core_health_with_slot", "存储槽位共享（退出五步关闭）"],
  ]) {
    if (!lib.includes(needle)) problems.push(`lib.rs 缺少${label}: ${needle}`);
  }

  const control = read("crates/aether-tauri/src/runtime_control.rs");
  for (const [needle, label] of [
    ["pub fn run_supervisor_startup", "启动序列收口入口"],
    ["cleanup_orphans", "孤儿清理（D5 三条件）"],
    ["warmup_all", "适配器预热"],
    ["spawn_monitors", "心跳/资源监控接线（T5b）"],
  ]) {
    if (!control.includes(needle)) problems.push(`runtime_control.rs 缺少${label}: ${needle}`);
  }

  const shutdown = read("crates/aether-tauri/src/shutdown.rs");
  for (const [needle, label] of [
    ["APP_EXIT_BUDGET: Duration = Duration::from_secs(10)", "T11 退出预算 10s"],
    ["pipeline.shutdown()", "广播 shutdown（D2 第一步）"],
    ["runtime.shutdown()", "适配器终止段（D5 序列）"],
    ["slot.take()", "存储运行时唯一消费者"],
    ["api.prevent_exit()", "退出前执行清理"],
  ]) {
    if (!shutdown.includes(needle)) problems.push(`shutdown.rs 缺少${label}: ${needle}`);
  }
  // D2 关闭序列顺序：广播 shutdown → 适配器终止段 → 存储五步。
  const shutdownOrder = [
    shutdown.indexOf("pipeline.shutdown()"),
    shutdown.indexOf("runtime.shutdown()"),
    shutdown.indexOf("slot.take()"),
  ];
  if (shutdownOrder.some((index) => index < 0)) {
    problems.push("shutdown.rs 缺少 D2 退出顺序三段（pipeline/runtime/storage）");
  } else if (shutdownOrder.join(",") !== [...shutdownOrder].sort((a, b) => a - b).join(",")) {
    problems.push("shutdown.rs 退出顺序必须与 D2 一致（广播 shutdown → 适配器终止段 → 存储五步）");
  }

  const processSource = read("crates/aether-adapters/src/process.rs");
  for (const [needle, label] of [
    ['TerminationStep::Force => "terminate_job_object"', "Windows 强杀机制（TerminateJobObject 优先）"],
    ['TerminationStep::Fallback => "taskkill_tree_force"', "taskkill /T /F 兜底"],
    ["JobObject", "Job Object 纳入（非沙箱）"],
    ["ProcessSession", "Unix setsid 进程组"],
  ]) {
    if (!processSource.includes(needle)) problems.push(`process.rs 缺少${label}: ${needle}`);
  }

  const fixtureSource = read("crates/aether-adapters/src/bin/aether-adapter-fixture.rs");
  for (const [needle, label] of [
    ["--survive-eof", "孤儿夹具（核心强杀后仍存活）"],
    ["Mode::Deaf", "不响应模式（T5b）"],
  ]) {
    if (!fixtureSource.includes(needle)) problems.push(`fixture 缺少${label}: ${needle}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：启动/退出接线与 D5 机制词典在案", problems.length === 0);
}

// ===== 6. 证据归档（DoD1/T11/T5b 原始 JSON + 汇总）=====

{
  mkdirSync(evidenceDir, { recursive: true });
  const writeEvidence = (name, value) => {
    if (value === null) return;
    writeFileSync(path.join(evidenceDir, name), `${JSON.stringify(value, null, 2)}\n`, "utf8");
  };
  writeEvidence("dod1-orphan-cleanup.json", dod1);
  writeEvidence("t11-exit-reliability.json", t11);
  writeEvidence("t5b-heartbeat-recovery.json", t5bEvidence);
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(
      {
        task: "M2-08",
        stamp,
        dod1,
        t11,
        t5b: t5bEvidence,
        thresholds: { t11_ms: 10_000, t5b_trigger_ms: 45_000, t5b_ready_ms: 120_000 },
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
  console.log(`[m2-08] 证据目录：${evidenceDir}`);
  record(
    "证据归档：DoD1/T11/T5b 原始 JSON 落盘（供 Gate 2 逐条出示）",
    existsSync(path.join(evidenceDir, "summary.json")) &&
      dod1 !== null &&
      t11 !== null &&
      t5bEvidence !== null,
  );
}

process.exit(summarize("verify-m2-08", checks));
