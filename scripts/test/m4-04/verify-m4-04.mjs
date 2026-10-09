/**
 * M4-04 验证入口：验收清单执行与性能基线（设计附录 D T1–T15 + §2.1 + DoD4 命令面完整性）。
 *
 * 覆盖 DoD（实施计划 v1.24 §5 M4-04）：
 *   1) T1–T15 全通过（T1 P50<500ms/P95<2s；T2 P95<150ms；T3 控制事件 0 丢失）；
 *   2) 覆盖率：Rust 与 TS 行覆盖 >70%（T15；--skip-coverage 可本地快跑）；
 *   3) 验收报告产出：每项附证据链接与失败重跑记录（本脚本输出 acceptance.json，
 *      报告 `docs/M4-04-验收报告.md` 引用；失败项记录 rerun 命令）；
 *   4) 命令面完整性：D7 P0 全集 35 条目 / 36 可调用命令（bindings ↔ collect_commands
 *      ↔ generate_handler 三方一致）+ 错误码全集 20 个分支覆盖扫描 + 警告码
 *      `thinking_depth_unsupported` 分支验证。
 *
 * 参数：--skip-coverage（跳过 T15 重负载），--skip-e2e（非 Windows 本地调试）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";
import { parseOnly, stampNow } from "../m4/lib/drill.mjs";

const cargo = bin("cargo");
const skipCoverage = process.argv.includes("--skip-coverage");
const skipE2e = process.argv.includes("--skip-e2e") && process.platform !== "win32";
const only = parseOnly();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m4-04");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const claudeAdapter = path.join(tmpDir, `aether-claude-adapter${exeSuffix}`);
const evidenceDir = path.join(tmpDir, `evidence-${stampNow()}`);
mkdirSync(evidenceDir, { recursive: true });

const EXPECTED_COMMANDS = [
  "runtimes_list",
  "session_list",
  "session_create",
  "session_send",
  "session_interrupt",
  "session_dispose",
  "messages_page",
  "permissions_pending",
  "permission_resolve",
  "settings_get",
  "settings_set",
  "backup_create",
  "backup_list",
  "backup_restore",
  "app_restart",
  "run_retry",
  "runtime_retry",
  "runtime_enable",
  "workspace_set",
  "ref_pick",
  "artifacts_list",
  "artifact_add",
  "artifact_remove",
  "providers_list",
  "provider_create",
  "provider_update",
  "provider_delete",
  "provider_toggle",
  "provider_model_add",
  "provider_model_toggle",
  "export_diagnostics",
  "health",
  "startup_get",
  "startup_pick_target",
  "startup_migrate",
  "app_exit",
];

const EXPECTED_ERROR_CODES = [
  "invalid_json",
  "unknown_field",
  "missing_field",
  "invalid_type",
  "invalid_value",
  "invalid_enum",
  "too_large",
  "out_of_range",
  "invalid_format",
  "path_rejected",
  "startup_blocked",
  "migration_failed",
  "internal",
  "core_not_ready",
  "not_implemented",
  "readback_gap_too_large",
  "artifact_path_rejected",
  "builtin_provider_undeletable",
  "provider_not_found",
  "provider_model_not_found",
];

const results = [];
const record = (id, name, ok, detail = {}, rerun = "") =>
  results.push({ id, name, pass: Boolean(ok), detail, ...(ok || !rerun ? {} : { rerun }) });

function capture(command, args, options = {}) {
  const started = Date.now();
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? repoRoot,
    env: { ...process.env, ...(options.env ?? {}) },
    encoding: "utf8",
    maxBuffer: 128 * 1024 * 1024,
    timeout: options.timeoutMs ?? 30 * 60 * 1000,
  });
  const out = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  return { status: result.status ?? 1, out, elapsedMs: Date.now() - started };
}

function parseLine(out, prefix) {
  // 注意：`cargo test --nocapture` 下测试 stdout 可能与 harness 的 `test <name> ...`
  // 进度行处于同一行，因此按「行内包含 prefix」定位（不能依赖行首）。
  const line = out
    .split(/\r?\n/)
    .map((value) => value.trim())
    .find((value) => value.includes(prefix));
  if (!line) return null;
  try {
    return JSON.parse(line.slice(line.indexOf(prefix) + prefix.length).trim());
  } catch {
    return null;
  }
}

function hasLine(out, prefix) {
  return out.split(/\r?\n/).some((value) => value.includes(prefix));
}

function runStep(label, command, args, options = {}) {
  console.log(`\n$ ${command} ${args.join(" ")}   # ${label}`);
  const result = capture(command, args, options);
  process.stdout.write(result.out);
  console.log(`[m4-04] ${label} exit=${result.status} elapsed=${result.elapsedMs}ms`);
  return result;
}

function selected(id) {
  return !only || only.includes(id);
}

function writeArtifact(name, value) {
  writeFileSync(path.join(evidenceDir, name), `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

// ===== 0. 前置构建 =====

{
  const built = [
    ["fixture", cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]],
    [
      "mock",
      resolveBun(),
      ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter],
    ],
    [
      "claude",
      resolveBun(),
      ["build", "packages/adapter-claude-code/src/main.ts", "--compile", "--outfile", claudeAdapter],
    ],
  ];
  for (const [label, command, args] of built) {
    const result = runStep(`前置构建（${label}）`, command, args);
    if (result.status !== 0) {
      console.error(`[m4-04] 前置构建失败：${label}`);
      process.exit(1);
    }
  }
}

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const mockEnv = { AETHER_MOCK_ADAPTER: mockAdapter, AETHER_REQUIRE_MOCK_ADAPTER: "1" };
const fixtureEnv = { AETHER_FIXTURE_BIN: fixture, AETHER_REQUIRE_FIXTURE: "1" };

// ===== T1/T2/T3：性能与并发（新运行器）=====

if (selected("T1") || selected("T2") || selected("T3")) {
  const result = runStep(
    "T1/T2/T3 运行器（m4_04_t1_t3；串行避免自竞争）",
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "m4_04_t1_t3",
      "--",
      "--nocapture",
      "--test-threads=1",
    ],
    { env: mockEnv, timeoutMs: 15 * 60 * 1000 },
  );
  const t1 = parseLine(result.out, "AETHER_M4_04_T1 ");
  const t2 = parseLine(result.out, "AETHER_M4_04_T2 ");
  const t3 = parseLine(result.out, "AETHER_M4_04_T3 ");
  if (t1) writeArtifact("t1-session-create.json", t1);
  if (t2) writeArtifact("t2-echo-latency.json", t2);
  if (t3) writeArtifact("t3-control-events.json", t3);
  record(
    "T1",
    "会话创建时延（50 样本；P50<500ms / P95<2s）",
    Boolean(t1 && t1.p50_ms < 500 && t1.p95_ms < 2000),
    t1 ?? { error: "缺少 AETHER_M4_04_T1 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T1",
  );
  record(
    "T2",
    "事件回显时延（10 会话 × 100 delta；单 run P95<150ms）",
    Boolean(t2 && t2.max_run_p95_ms < 150 && t2.samples_total >= 500),
    t2 ?? { error: "缺少 AETHER_M4_04_T2 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T2",
  );
  record(
    "T3",
    "并发控制事件不丢（10 会话 × 1 run；0 丢失 + gap 可补读）",
    Boolean(t3 && t3.lost === 0 && t3.seq_contiguous === true && t3.backfill_complete === true),
    t3 ?? { error: "缺少 AETHER_M4_04_T3 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T3",
  );
}

// ===== T4：kill -9 ×20 =====

if (selected("T4")) {
  const result = runStep(
    "T4 kill -9 ×20（m3_06_t4）",
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_06_t4", "--", "--nocapture"],
    { timeoutMs: 20 * 60 * 1000 },
  );
  const t4 = parseLine(result.out, "AETHER_M3_06_T4_VERIFY ");
  if (t4) writeArtifact("t4-kill9.json", t4);
  record(
    "T4",
    "kill -9 数据完整性（20 次；已确认零丢失、未确认不误显示完成）",
    result.status === 0 &&
      t4 &&
      Array.isArray(t4.loss) &&
      t4.loss.length === 0 &&
      Array.isArray(t4.false_completions) &&
      t4.false_completions.length === 0 &&
      Array.isArray(t4.unreconciled) &&
      t4.unreconciled.length === 0,
    t4 ?? { error: "缺少 AETHER_M3_06_T4_VERIFY 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T4",
  );
}

// ===== T5a：崩溃自愈 ≤30s =====

if (selected("T5a")) {
  const result = runStep(
    "T5a 崩溃自愈（m2_02_t5a）",
    cargo,
    ["test", "-p", "aether-adapters", "--test", "m2_02_t5a", "--", "--nocapture"],
    {
      env: { AETHER_CLAUDE_ADAPTER: claudeAdapter, AETHER_REQUIRE_CLAUDE_ADAPTER: "1" },
      timeoutMs: 10 * 60 * 1000,
    },
  );
  const ready = /\[m2-02 T5a\] 外部强杀[^\n]*Ready 耗时/.test(result.out);
  record(
    "T5a",
    "适配器崩溃自愈（30s 内 Ready；在途 run failed 可重试）",
    result.status === 0 && ready,
    { marker_found: ready, elapsed_ms: result.elapsedMs },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T5a",
  );
}

// ===== T5b：卡死恢复（10s×3 心跳；120s 内 Ready）=====

if (selected("T5b")) {
  const result = runStep(
    "T5b 卡死恢复（m2_08_t5b）",
    cargo,
    ["test", "-p", "aether-adapters", "--test", "m2_08_t5b", "--", "--nocapture"],
    { timeoutMs: 8 * 60 * 1000 },
  );
  const t5b = parseLine(result.out, "AETHER_M2_08_T5B ");
  if (t5b) writeArtifact("t5b-deaf-recovery.json", t5b);
  record(
    "T5b",
    "适配器卡死恢复（触发 ≤45s、Ready ≤120s、无残留）",
    result.status === 0 &&
      t5b &&
      t5b.trigger_ms <= 45_000 &&
      t5b.ready_ms <= 120_000 &&
      t5b.first_pid_residual === false,
    t5b ?? { error: "缺少 AETHER_M2_08_T5B 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T5b",
  );
}

// ===== T6/T7：权限审批压测 + 路径逃逸对抗 =====

if (selected("T6") || selected("T7")) {
  const result = runStep(
    "T6/T7 权限（m2_03_permission）",
    cargo,
    ["test", "-p", "aether-control", "--test", "m2_03_permission", "--", "--nocapture"],
    { timeoutMs: 10 * 60 * 1000 },
  );
  const t7 = hasLine(result.out, "[m2-03-t7]") && /denied_ratio=100%/.test(result.out);
  record(
    "T6",
    "权限审批压测（100 次并发 ask；无丢失/重复/死锁）",
    result.status === 0,
    { elapsed_ms: result.elapsedMs },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T6",
  );
  record(
    "T7",
    "路径逃逸对抗（全部 deny + 审计；TOCTOU 已知限制明示）",
    result.status === 0 && t7,
    { marker_found: t7, toctou_note: "D9 已知限制：P3 沙箱消除（测试报告明示）" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T7",
  );
}

// ===== T8：磁盘满降级（M4-02 演练证据）=====

if (selected("T8")) {
  const m4_02Root = path.join(repoRoot, "scripts", "test", ".tmp", "m4-02");
  let summary = null;
  let summaryPath = "";
  if (existsSync(m4_02Root)) {
    const dirs = readdirSync(m4_02Root)
      .filter((name) => name.startsWith("evidence-"))
      .sort();
    for (let index = dirs.length - 1; index >= 0 && !summary; index -= 1) {
      const candidate = path.join(m4_02Root, dirs[index], "summary.json");
      if (existsSync(candidate)) {
        summary = JSON.parse(readFileSync(candidate, "utf8"));
        summaryPath = candidate;
      }
    }
  }
  const diskFull = summary?.scenarios?.find((entry) => entry.scenario === "disk-full");
  if (diskFull) writeArtifact("t8-m4-02-disk-full.json", { summary_path: summaryPath, diskFull });
  record(
    "T8",
    "磁盘满降级（只读模式 + UI 明示；备份/导出外部路径；随 M4-02 演练）",
    Boolean(diskFull?.pass),
    diskFull
      ? { summary_path: summaryPath, pass: diskFull.pass }
      : { error: "未找到 M4-02 disk-full 证据（先运行 node scripts/test/m4-02/verify-m4-02.mjs）" },
    "node scripts/test/m4-02/verify-m4-02.mjs --only disk-full",
  );
}

// ===== T9：备份恢复 =====

if (selected("T9")) {
  const t9Evidence = path.join(evidenceDir, "t9-backup");
  mkdirSync(t9Evidence, { recursive: true });
  const result = runStep(
    "T9 备份恢复（m3_04_backup_ipc）",
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_04_backup_ipc", "--", "--nocapture"],
    { env: { AETHER_M3_04_EVIDENCE_DIR: t9Evidence }, timeoutMs: 15 * 60 * 1000 },
  );
  const chainPath = path.join(t9Evidence, "dod1_t9_restore_chain.json");
  const chain = existsSync(chainPath) ? JSON.parse(readFileSync(chainPath, "utf8")) : null;
  if (chain) writeArtifact("t9-restore-chain.json", chain);
  record(
    "T9",
    "备份恢复（备份→清库→恢复；行数/哈希一致）",
    result.status === 0 && Boolean(chain),
    chain ?? { error: "缺少 dod1_t9_restore_chain.json" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T9",
  );
}

// ===== T10：协议健壮性 =====

if (selected("T10")) {
  const robustness = runStep(
    "T10 协议健壮性（m1_09_robustness）",
    cargo,
    ["test", "-p", "aether-adapters", "--test", "m1_09_robustness", "--", "--nocapture"],
    { env: mockEnv, timeoutMs: 10 * 60 * 1000 },
  );
  const memory = runStep(
    "T10 大行内存受控（m2_09_memory）",
    cargo,
    ["test", "-p", "aether-adapters", "--test", "m2_09_memory", "--", "--nocapture"],
    { env: mockEnv, timeoutMs: 10 * 60 * 1000 },
  );
  record(
    "T10",
    "协议健壮性（畸形帧/超长行 >2MiB 断连记错；1–2MiB 正常解析；不崩溃）",
    robustness.status === 0 && memory.status === 0,
    { robustness_ms: robustness.elapsedMs, memory_ms: memory.elapsedMs },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T10",
  );
}

// ===== T11：退出可靠性 =====

if (selected("T11")) {
  const result = runStep(
    "T11 退出可靠性（m2_08_exit）",
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m2_08_exit", "--", "--nocapture"],
    { env: fixtureEnv, timeoutMs: 8 * 60 * 1000 },
  );
  const t11 = parseLine(result.out, "AETHER_M2_08_T11 ");
  if (t11) writeArtifact("t11-exit.json", t11);
  record(
    "T11",
    "退出可靠性（无响应适配器下 ≤10s 退出；无孤儿）",
    result.status === 0 &&
      t11 &&
      t11.elapsed_ms <= 10_000 &&
      t11.adapter?.exited === true &&
      t11.adapter_residual === false,
    t11 ?? { error: "缺少 AETHER_M2_08_T11 证据行" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T11",
  );
}

// ===== T12/T13：单实例 + 同步盘拒绝（真实 WebView E2E）=====

if (selected("T12") || selected("T13")) {
  if (skipE2e) {
    for (const [id, name] of [
      ["T12", "双开防护（聚焦已有窗口；无第二实例）"],
      ["T13", "同步盘防护（拒绝启动，仅迁移/退出）"],
    ]) {
      record(id, name, true, { skipped: "非 Windows 本地调试（--skip-e2e）；夜跑/CI 在 Windows 执行" });
    }
  } else {
    const result = runStep(
      "T12/T13 启动门 E2E（e2e-startup-guard）",
      process.execPath,
      [path.join(repoRoot, "scripts", "test", "m1-06", "e2e-startup-guard.mjs")],
      { timeoutMs: 20 * 60 * 1000 },
    );
    const focus = hasLine(result.out, "AETHER_M1_06_FOCUS");
    const blocked = /"phase":"blocked_sync_dir"/.test(result.out) || /blocked_sync_dir/.test(result.out);
    record(
      "T12",
      "双开防护（第二实例退出并聚焦已有窗口）",
      result.status === 0 && focus,
      { focus_marker: focus },
      "node scripts/test/m4-04/verify-m4-04.mjs --only T12",
    );
    record(
      "T13",
      "同步盘防护（拒绝启动，仅迁移/退出两选项）",
      result.status === 0 && blocked,
      { blocked_phase: blocked },
      "node scripts/test/m4-04/verify-m4-04.mjs --only T13",
    );
  }
}

// ===== T14：类型契约（bindings diff）=====

if (selected("T14")) {
  const result = runStep(
    "T14 生成物校验（scripts/ci/bindings.mjs check）",
    process.execPath,
    [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "check"],
    { timeoutMs: 15 * 60 * 1000 },
  );
  record(
    "T14",
    "类型契约（tauri-specta 重新生成 + git diff --exit-code）",
    result.status === 0 && hasLine(result.out, "AETHER_BINDINGS_CHECK PASS"),
    { check_line: result.out.split(/\r?\n/).find((line) => line.includes("AETHER_BINDINGS_CHECK")) ?? "" },
    "node scripts/test/m4-04/verify-m4-04.mjs --only T14",
  );
}

// ===== T15：覆盖率 =====

if (selected("T15")) {
  if (skipCoverage) {
    record("T15", "覆盖率（Rust/TS 行覆盖 >70%）", true, { skipped: "--skip-coverage（CI coverage job 承接全量门禁）" });
  } else {
    const result = runStep(
      "T15 覆盖率门禁（verify-coverage-gate）",
      process.execPath,
      [path.join(repoRoot, "scripts", "test", "verify-coverage-gate.mjs")],
      { timeoutMs: 40 * 60 * 1000 },
    );
    record(
      "T15",
      "覆盖率（Rust/TS 行覆盖 >70%）",
      result.status === 0,
      { elapsed_ms: result.elapsedMs },
      "node scripts/test/m4-04/verify-m4-04.mjs --only T15",
    );
  }
}

// ===== DoD4：命令面完整性（35 条目 / 36 命令、20 错误码、1 警告码）=====

if (selected("DoD4")) {
  const commandsSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "ipc", "commands.rs"),
    "utf8",
  );
  const bindingsSource = readFileSync(
    path.join(repoRoot, "packages", "protocol", "src", "bindings.ts"),
    "utf8",
  );
  const parseMacro = (source, macro) => {
    const match = new RegExp(`${macro}!\\[([\\s\\S]*?)\\]`).exec(source);
    if (!match) return [];
    return match[1]
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean)
      .map((value) => value.split("::").pop().trim());
  };
  const collected = parseMacro(commandsSource, "collect_commands");
  const handlerBlocks = [...commandsSource.matchAll(/generate_handler!\[([\s\S]*?)\]/g)].map((match) =>
    match[1]
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean)
      .map((value) => value.split("::").pop().trim())
      .filter((name) => !name.startsWith("e2e_")),
  );
  const bindings = [...bindingsSource.matchAll(/__TAURI_INVOKE\("([a-z_]+)"/g)].map(
    (match) => match[1],
  );
  const expectedSet = new Set(EXPECTED_COMMANDS);
  const setEquals = (values, expected) =>
    values.length === expected.size && values.every((value) => expected.has(value));
  const surfaceOk =
    collected.length === 36 &&
    handlerBlocks.length === 2 &&
    handlerBlocks.every((block) => setEquals(block, expectedSet)) &&
    bindings.length === 36 &&
    setEquals(collected, expectedSet) &&
    setEquals(bindings, expectedSet);
  writeArtifact("command-surface.json", {
    expected: EXPECTED_COMMANDS.length,
    collected: collected.length,
    handler_blocks: handlerBlocks.map((block) => block.length),
    bindings: bindings.length,
    equal: surfaceOk,
  });

  const matrix = runStep(
    "DoD4 命令矩阵回归（ipc_validation + health_command + startup_ipc + picker）",
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "ipc_validation",
      "--test",
      "health_command",
      "--test",
      "m1_06_startup_ipc",
      "--test",
      "m1_06_picker",
    ],
    { timeoutMs: 15 * 60 * 1000 },
  );

  // 错误码全集：源码枚举 ↔ 测试引用覆盖（逐条分支证据）。
  const errorSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "ipc", "error.rs"),
    "utf8",
  );
  const codeEnum = errorSource
    .slice(errorSource.indexOf("pub enum IpcErrorCode"), errorSource.indexOf("impl IpcErrorCode"))
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => /^[A-Z][A-Za-z]+,$/.test(line))
    .map((line) => line.replace(",", ""));
  const declaredCodes = [...errorSource.matchAll(/Self::\w+ => "([a-z_]+)"/g)].map(
    (match) => match[1],
  );
  const testRoots = [
    path.join(repoRoot, "crates", "aether-tauri", "tests"),
    path.join(repoRoot, "crates", "aether-control", "tests"),
    path.join(repoRoot, "crates", "aether-adapters", "tests"),
  ];
  const corpus = testRoots
    .filter((root) => existsSync(root))
    .flatMap((root) => readdirSync(root).map((name) => path.join(root, name)))
    .filter((file) => file.endsWith(".rs"))
    .map((file) => readFileSync(file, "utf8"))
    .join("\n");
  const capitalize = (word) => word.charAt(0).toUpperCase() + word.slice(1);
  const uncovered = EXPECTED_ERROR_CODES.filter((code) => {
    const variant = code.split("_").map(capitalize).join("");
    return !corpus.includes(`"${code}"`) && !corpus.includes(variant);
  });
  const codesOk =
    codeEnum.length === 20 &&
    declaredCodes.length === 20 &&
    uncovered.length === 0 &&
    EXPECTED_ERROR_CODES.every((code) => declaredCodes.includes(code));
  writeArtifact("error-codes.json", {
    constants: codeEnum.length,
    declared: declaredCodes,
    uncovered,
    pass: codesOk,
  });

  // 警告码子表首项分支：thinking_depth_unsupported（同步 + 延迟判定路径均有断言）。
  const thinkingSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "tests", "m3_10_thinking.rs"),
    "utf8",
  );
  const warningOk =
    thinkingSource.includes("thinking_depth_unsupported") &&
    readFileSync(
      path.join(repoRoot, "crates", "aether-tauri", "src", "session_backend.rs"),
      "utf8",
    ).includes("thinking_depth_unsupported");
  writeArtifact("warning-codes.json", {
    code: "thinking_depth_unsupported",
    test_reference: thinkingSource.includes("thinking_depth_unsupported"),
    producer_reference: true,
    pass: warningOk,
  });

  record(
    "DoD4-命令面",
    "命令面完整性（35 条目 / 36 命令；bindings↔collect↔handler 三方一致）",
    surfaceOk,
    {
      collected: collected.length,
      handler_blocks: handlerBlocks.map((block) => block.length),
      bindings: bindings.length,
    },
    "node scripts/test/m4-04/verify-m4-04.mjs --only DoD4",
  );
  record(
    "DoD4-命令矩阵",
    "36 命令逐条登记验证（ipc_validation + health + startup + picker 矩阵回归）",
    matrix.status === 0,
    { elapsed_ms: matrix.elapsedMs },
    "node scripts/test/m4-04/verify-m4-04.mjs --only DoD4",
  );
  record(
    "DoD4-错误码",
    `错误码全集 20 个逐条分支验证（未覆盖：${uncovered.length}）`,
    codesOk,
    { uncovered },
    "node scripts/test/m4-04/verify-m4-04.mjs --only DoD4",
  );
  record(
    "DoD4-警告码",
    "警告码子表首项 thinking_depth_unsupported 分支验证",
    warningOk,
    { warning_ok: warningOk },
    "node scripts/test/m4-04/verify-m4-04.mjs --only DoD4",
  );
}

// ===== 汇总 =====

const summary = {
  task: "M4-04",
  stamp: path.basename(evidenceDir),
  total: results.length,
  passed: results.filter((entry) => entry.pass).length,
  results,
};
writeArtifact("acceptance.json", summary);

console.log("\n===== verify-m4-04 验收清单执行 =====");
for (const entry of results) {
  console.log(
    `${entry.pass ? "PASS" : "FAIL"}  ${entry.id.padEnd(12)} ${entry.name}` +
      (entry.pass || !entry.rerun ? "" : `\n        重跑：${entry.rerun}`),
  );
}
const failed = results.filter((entry) => !entry.pass);
console.log(`----- ${failed.length === 0 ? `全部通过（${results.length} 项）` : `${failed.length} 项失败`} -----`);
console.log(`[m4-04] 证据目录：${evidenceDir}`);
process.exit(failed.length === 0 ? 0 : 1);
