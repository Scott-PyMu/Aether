#!/usr/bin/env node
/**
 * `pnpm verify:m9-13`（P2 计划 §2.13 / §2.15 G7；ADR-029 v0.4 §10）。
 *
 * 校验：
 *  1. 脚本/映射/阈值冻结面；
 *  2. 性能证据（启动/RSS/10×50/体积/WAL）逐项硬阈值；
 *  3. 数据证据（内测数据单向打开、schema 不变、备份恢复、安全模式维护例外、
 *     Keychain/S-13、WAL）；
 *  4. 全量回归/P0/P1 映射（T1–T15、P1 三项、T14 生成物、M1-08/M4-05 映射）；
 *  5. 覆盖率（前端不下降；Rust 增量；G8 全量对比 pending/完成）；
 *  6. 平台矩阵（Win/mac/Linux；未跑项必须标注原因，不得静默跳过）；
 *  7. `docs/M9-13-证据.md` 与迁移 0001–0009 不变。
 *
 * 用法：node scripts/test/m9-13/verify-m9-13.mjs
 */
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
let failed = 0;

function check(name, ok, detail = "") {
  checks.push({ name, ok, detail });
  if (ok) {
    console.log(`PASS  ${name}${detail ? `  ${detail}` : ""}`);
  } else {
    failed += 1;
    console.error(`FAIL  ${name}${detail ? `  ${detail}` : ""}`);
  }
}

function note(name, detail = "") {
  checks.push({ name, ok: true, note: true, detail });
  console.log(`NOTE  ${name}${detail ? `  ${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(repoRoot, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function readJsonIfExists(rel) {
  if (!existsSync(path.join(repoRoot, rel))) return null;
  try {
    return readJson(rel);
  } catch {
    return null;
  }
}

const pkg = readJson("package.json");
for (const [name, command] of [
  ["verify:m9-13", "scripts/test/m9-13/verify-m9-13.mjs"],
  ["m9-13:performance", "scripts/test/m9-13/performance.mjs"],
  ["m9-13:data", "scripts/test/m9-13/data-drill.mjs"],
  ["m9-13:regression", "scripts/test/m9-13/regression.mjs"],
  ["coverage:m9-13", "scripts/test/m9-13/coverage-compare.mjs"],
]) {
  check(`package.json scripts.${name}`, pkg.scripts?.[name]?.includes(command) === true, pkg.scripts?.[name] ?? "缺失");
}

// ===== 1. P0/P1 映射 =====
const mapping = readJson("scripts/test/m9-13/p0-p1-mapping.json");
const p0Ids = new Set((mapping.p0 ?? []).map((item) => item.id));
for (let index = 1; index <= 15; index += 1) {
  const id = `T${index}`;
  const covered =
    index === 5 ? p0Ids.has("T5a") && p0Ids.has("T5b") : p0Ids.has(id);
  check(
    `P0 映射覆盖 ${id}`,
    covered,
    (mapping.p0 ?? []).map((item) => item.id).join(","),
  );
}
check(
  "P1 映射覆盖三运行时/加密备份/规则误配率",
  (mapping.p1 ?? []).length === 3 &&
    mapping.p1.some((item) => item.id.includes("三运行时")) &&
    mapping.p1.some((item) => item.id.includes("加密备份")) &&
    mapping.p1.some((item) => item.id.includes("规则误配")),
  `${(mapping.p1 ?? []).length} 项`,
);
const mappingCommands = [...(mapping.p0 ?? []), ...(mapping.p1 ?? [])];
for (const item of mappingCommands) {
  const command = item.command ?? "";
  const testNames = [...command.matchAll(/--test\s+([a-z0-9_]+)/g)].map((match) => match[1]);
  for (const testName of testNames) {
    const candidates = readdirSync(path.join(repoRoot, "crates"), { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => path.join(repoRoot, "crates", entry.name, "tests", `${testName}.rs`));
    check(`映射 ${item.id} 测试目标存在：${testName}`, candidates.some((file) => existsSync(file)));
  }
  const scripts = [...command.matchAll(/(scripts\/[A-Za-z0-9_./-]+\.mjs)/g)].map((match) => match[1]);
  for (const script of scripts) {
    check(`映射 ${item.id} 脚本存在：${script}`, existsSync(path.join(repoRoot, script)));
  }
}

// ===== 2. 阈值冻结面 =====
const thresholds = readJson("docs/M9-01-baseline/freeze/thresholds.json");
const expected = {
  installer_max_bytes: 191717376,
  absolute_cap_bytes: 471859200,
  idle_rss_max_bytes: 629145600,
  ten_sessions_max_bytes: 1258291200,
  window_shown_max_ms: 1500,
  handshake_target_ms: 2000,
  handshake_timeout_ms: 10000,
  ui_ready_max_ms: 8000,
  session_create_max_ms: 2000,
  p95_latency_max_ms: 150,
};
const actual = {
  installer_max_bytes: thresholds.installer?.max_bytes,
  absolute_cap_bytes: thresholds.installer?.absolute_cap_bytes,
  idle_rss_max_bytes: thresholds.runtime?.idle_rss_max_bytes,
  ten_sessions_max_bytes: thresholds.runtime?.ten_sessions_max_bytes,
  window_shown_max_ms: thresholds.startup?.window_shown_max_ms,
  handshake_target_ms: thresholds.startup?.handshake_target_ms,
  handshake_timeout_ms: thresholds.startup?.handshake_timeout_ms,
  ui_ready_max_ms: thresholds.startup?.ui_ready_max_ms,
  session_create_max_ms: thresholds.startup?.session_create_max_ms,
  p95_latency_max_ms: thresholds.events?.p95_latency_max_ms,
};
for (const [key, value] of Object.entries(expected)) {
  check(`冻结阈值 ${key}=${value}`, actual[key] === value, String(actual[key]));
}

// ===== 3. 性能证据 =====
const performance = readJsonIfExists("scripts/test/.tmp/m9-13/performance/performance.json");
check("性能证据存在", performance !== null, "scripts/test/.tmp/m9-13/performance/performance.json");
if (performance) {
  const metrics = performance.metrics ?? {};
  const packaged = metrics.packaged ?? null;
  const launch = packaged?.first_launch ?? metrics;
  const tenSessionsBytes =
    packaged?.ten_sessions_upper_bound_bytes ?? metrics.ten_sessions_rss_bytes;
  const tenSessionsMib =
    packaged?.ten_sessions_upper_bound_mib ?? metrics.ten_sessions_rss_mib;
  check("性能证据 passed", performance.passed === true, String(performance.passed));
  check(
    "窗口可见 ≤1.5s（安装产物优先）",
    Number(launch.window_visible_ms) > 0 &&
      Number(launch.window_visible_ms) <= thresholds.startup.window_shown_max_ms,
    `${launch.window_visible_ms}ms`,
  );
  check(
    "initialize 目标 ≤2s 且硬超时 ≤10s",
    Number(metrics.initialize_ms) > 0 &&
      Number(metrics.initialize_ms) <= thresholds.startup.handshake_target_ms &&
      Number(metrics.initialize_ms) <= thresholds.startup.handshake_timeout_ms,
    `${metrics.initialize_ms}ms`,
  );
  check(
    "UI ready ≤8s（安装产物优先）",
    Number(launch.ui_ready_ms) > 0 &&
      Number(launch.ui_ready_ms) <= thresholds.startup.ui_ready_max_ms,
    `${launch.ui_ready_ms}ms`,
  );
  check(
    "会话创建 <2s",
    Number(metrics.session_create_ms) >= 0 &&
      Number(metrics.session_create_ms) < thresholds.startup.session_create_max_ms,
    `${metrics.session_create_ms}ms`,
  );
  check(
    "idle RSS ≤600MiB（安装产物优先）",
    Number(launch.idle_rss_bytes) > 0 &&
      Number(launch.idle_rss_bytes) <= thresholds.runtime.idle_rss_max_bytes,
    `${launch.idle_rss_mib} MiB`,
  );
  check(
    "10 会话 RSS ≤1.2GiB（安装产物优先）",
    Number(tenSessionsBytes) > 0 &&
      Number(tenSessionsBytes) <= thresholds.runtime.ten_sessions_max_bytes,
    `${tenSessionsMib} MiB`,
  );
  check("10 会话可创建/列出", Number(metrics.sessions) >= 10, `sessions=${metrics.sessions}`);
  const installerBytes = packaged?.installer?.bytes ?? metrics.installer?.bytes;
  check(
    "安装包体积 ≤1.5×HEAD 且 ≤450MiB",
    installerBytes !== null &&
      installerBytes !== undefined &&
      Number(installerBytes) <= thresholds.installer.max_bytes &&
      Number(installerBytes) <= thresholds.installer.absolute_cap_bytes,
    installerBytes ? `${installerBytes} B` : "缺失",
  );
  const ten = metrics.ten_by_fifty;
  check(
    "10×50 控制事件 0 丢失",
    ten?.passed === true && ten?.report?.control_lost === 0,
    ten?.report
      ? `control=${ten.report.control_received}/${ten.report.control_expected}`
      : ten?.reason ?? "缺失",
  );
  check(
    "10×50 P95 <150ms",
    ten?.passed === true && Number(ten?.report?.p95_ms) < thresholds.events.p95_latency_max_ms,
    `${ten?.report?.p95_ms ?? "?"}ms`,
  );
  check(
    "关闭后 WAL checkpoint ≤4KiB",
    Number(metrics.wal_bytes_after_shutdown) <= 4096,
    `${metrics.wal_bytes_after_shutdown} B`,
  );
}

// ===== 4. 数据证据 =====
const data = readJsonIfExists("scripts/test/.tmp/m9-13/data/data-drill.json");
check("数据证据存在", data !== null, "scripts/test/.tmp/m9-13/data/data-drill.json");
if (data) {
  check("数据证据 passed", data.passed === true, String(data.passed));
  check("内测数据 Electron 单向打开", data.one_way_electron_open === true, String(data.one_way_electron_open));
  check("无反向 Tauri 打开/跨壳往返", data.reverse_tauri_open === false, String(data.reverse_tauri_open));
  check("无 schema 变更", data.schema_unchanged === true, String(data.schema_unchanged));
  check(
    "备份恢复：设置回滚 + quick_check ok",
    data.backup_restore?.setting_restored === true && data.backup_restore?.quick_check === "ok",
    JSON.stringify(data.backup_restore ?? {}),
  );
  check("安全模式维护例外测试通过", data.safe_mode?.exit === 0, `exit=${data.safe_mode?.exit}`);
  check(
    "Keychain preflight（ok/reauth）",
    data.keychain?.mode === "ok" || data.keychain?.mode === "reauth",
    `mode=${data.keychain?.mode}`,
  );
  check(
    "Keychain/S-13 回归通过",
    data.keychain?.m9_09_test_exit === 0 && data.keychain?.unit_test_exit === 0,
    `test=${data.keychain?.m9_09_test_exit} unit=${data.keychain?.unit_test_exit}`,
  );
  check(
    "WAL checkpoint ≤4KiB",
    Number(data.wal_bytes_after_shutdown) <= 4096,
    `${data.wal_bytes_after_shutdown} B`,
  );
  for (const key of ["sessions", "messages", "settings", "presets", "workspaces", "audit_log", "providers", "backups"]) {
    const count = data.source_snapshot?.counts?.[key];
    check(
      `内测数据 ${key} 可读`,
      typeof count === "number" && count >= 0,
      String(count ?? "缺失"),
    );
  }
}

// ===== 5. 全量回归/P0/P1 =====
const regression = readJsonIfExists("scripts/test/.tmp/m9-13/regression/results.json");
check("回归证据存在", regression !== null, "scripts/test/.tmp/m9-13/regression/results.json");
if (regression) {
  const byId = new Map((regression.commands ?? []).map((item) => [item.id, item]));
  const required = [
    "workspace-rust",
    "frontend-test",
    "frontend-typecheck",
    "frontend-build",
    "clippy",
    "deny",
    "baseline",
    "protocol",
    "error-codes",
    "security",
    "m9-12-verify",
    "m9-12-prepare-e2e",
    "m9-12-e2e",
    "smoke-installer",
    "T4",
    "T5a",
    "T5b",
    "T6",
    "T10",
    "T11",
    "T12",
    "T13",
    "P1-three-runtimes",
    "P1-encrypted-backup",
    "P1-rule-misconfiguration",
  ];
  for (const id of required) {
    const item = byId.get(id);
    if (!item) {
      check(`回归 ${id}`, false, "缺失");
    } else {
      check(`回归 ${id}`, item.ok === true, `exit=${item.exit}`);
    }
  }
  check("回归总 passed", regression.passed === true, String(regression.passed));
  for (const id of ["protocol", "error-codes", "baseline"]) {
    const item = byId.get(id);
    check(`T14 映射 ${id}`, item?.ok === true, item ? `exit=${item.exit}` : "缺失");
  }
  check("M1-08 → M9-10 verify:security", byId.get("security")?.ok === true);
  check("M4-05/ADR-001 → M9-11 smoke:installer", byId.get("smoke-installer")?.ok === true);
}

const manifest = readJsonIfExists("packages/protocol/src/command-manifest.json");
if (manifest) {
  const commands = manifest.commands ?? manifest;
  const total = Array.isArray(commands) ? commands.length : commands.total;
  const daemon = Array.isArray(commands)
    ? commands.filter((item) => item.route === "daemon").length
    : commands.daemon;
  const shell = Array.isArray(commands)
    ? commands.filter((item) => item.route === "shell").length
    : commands.shell;
  const ui = Array.isArray(commands)
    ? commands.filter((item) => item.exposure === "ui").length
    : commands.ui;
  check("T14 命令合同 63", total === 63, String(total));
  check("T14 daemon 59 / shell 4", daemon === 59 && shell === 4, `${daemon}/${shell}`);
  check("T14 UI 暴露 61", ui === 61, String(ui));
} else {
  check("T14 command-manifest 存在", false);
}

// ===== 6. 覆盖率 =====
const coverage = readJsonIfExists("scripts/test/.tmp/m9-13/coverage/coverage.json");
check("覆盖率证据存在", coverage !== null, "scripts/test/.tmp/m9-13/coverage/coverage.json");
if (coverage) {
  check("覆盖率证据 passed", coverage.passed === true, String(coverage.passed));
  check(
    "前端覆盖率不下降",
    (coverage.checks ?? []).filter((item) => item.name.startsWith("前端覆盖率不下降")).every((item) => item.ok),
    (coverage.checks ?? []).filter((item) => item.name.startsWith("前端覆盖率不下降")).length + " 项",
  );
  check(
    "Rust 增量覆盖已归档",
    coverage.rust?.incremental !== null && coverage.rust?.incremental !== undefined,
    coverage.rust?.incremental?.status ?? "缺失",
  );
  // G8/M9-14：Tauri 已退役，删除清单应为 0（历史 128 条已在 M9-13 证据归档）。
  check(
    "Tauri 退役后删除清单为 0",
    Number(coverage.rust?.tauri_deletion_count) === 0,
    String(coverage.rust?.tauri_deletion_count ?? "缺失"),
  );
  const g8 = coverage.rust?.g8_full;
  check(
    "G8 全量覆盖率对比已完成（full.json）",
    Boolean(g8?.source),
    g8?.status ?? g8?.source ?? "缺失",
  );
}

// ===== 7. 平台矩阵 =====
const platform = readJsonIfExists("scripts/test/m9-13/platform-matrix.json");
check("平台矩阵存在", platform !== null, "scripts/test/m9-13/platform-matrix.json");
if (platform) {
  const platforms = platform.platforms ?? [];
  for (const id of ["windows", "macos", "linux"]) {
    const item = platforms.find((entry) => entry.id === id);
    check(`平台矩阵 ${id}`, Boolean(item), item?.status ?? "缺失");
    if (item && item.status !== "passed") {
      check(`平台矩阵 ${id} 未跑原因`, Boolean(item.reason), item.reason ?? "缺失");
      note(`平台 ${id} 状态=${item.status}`, item.ci_run ? `CI run ${item.ci_run}` : item.reason ?? "");
    }
  }
}

// ===== 8. 文档/迁移 =====
check("M9-13 证据文档存在", existsSync(path.join(repoRoot, "docs/M9-13-证据.md")));
check(
  "macOS 签名/公证密钥登记文档存在",
  existsSync(path.join(repoRoot, "docs/M9-13-macOS-密钥登记.md")),
);
if (existsSync(path.join(repoRoot, "docs/M9-13-证据.md"))) {
  const doc = read("docs/M9-13-证据.md");
  for (const section of ["P0/P1", "内测数据", "性能", "平台矩阵", "覆盖率", "安全模式"]) {
    check(`证据文档包含「${section}」`, doc.includes(section));
  }
}
const migrations = readdirSync(path.join(repoRoot, "migrations"))
  .filter((name) => /^\d{4}_.*\.sql$/.test(name))
  .sort();
check(
  "迁移文件 0001–0009 未新增/未改名",
  JSON.stringify(migrations) ===
    JSON.stringify([
      "0001_init.sql",
      "0002_unique_keys.sql",
      "0003_p0_ui_extensions.sql",
      "0004_permission_mode_and_rules.sql",
      "0005_audit_indexes.sql",
      "0006_runtime_enabled.sql",
      "0007_presets.sql",
      "0008_session_search_fts.sql",
      "0009_provider_endpoints.sql",
    ]),
  migrations.join(","),
);
const baselineManifest = readJson("docs/M9-01-baseline/manifest.json");
const contractArtifacts = (baselineManifest.artifacts ?? []).filter((entry) =>
  entry.path.startsWith("docs/M9-01-baseline/contract/"),
);
check(
  "M9-01 基线 manifest 仍为 63 命令",
  baselineManifest.commands?.length === 63 && baselineManifest.counts?.commands === 63,
  `${baselineManifest.commands?.length ?? 0}`,
);
note("基线 contract 摘要", `${contractArtifacts.length} 个 contract 产物`);

// ===== 9. 无 Tauri 回退包/跨壳（M9-13 不切换，只断言未引入） =====
const packageJsonText = read("package.json");
check("无 Tauri 回退包脚本", !/tauri.*fallback|fallback.*tauri/i.test(packageJsonText));
check(
  "M9-13 脚本不启动 Tauri",
  !/AETHER_SHELL\s*=\s*["']?tauri/i.test(
    read("scripts/test/m9-13/performance.mjs") +
      read("scripts/test/m9-13/data-drill.mjs") +
      read("scripts/test/m9-13/regression.mjs"),
  ),
);

console.log("");
console.log(
  `[verify:m9-13] ${failed === 0 ? "PASS" : `FAIL（${failed} 项）`} 检查=${checks.length}` +
    (checks.filter((item) => item.note).length > 0
      ? ` 备注=${checks.filter((item) => item.note).length}`
      : ""),
);
if (failed > 0) process.exit(1);
console.log("AETHER_M9_13 PASS");
process.exit(0);
