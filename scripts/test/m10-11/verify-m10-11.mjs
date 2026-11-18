#!/usr/bin/env node
/**
 * `pnpm verify:m10-11` —— 保留清理与文件收缩（ADR-039 v1.0 D-039-4）可复跑检查。
 *
 * 检查面：
 *  1. 产品选批 `(type, ts)` / `idx_events_type_ts` / 无 `ORDER BY rowid`；
 *  2. 删除事务档位矩阵 500/1000/2000/5000 与 P95 <500ms（docs/M10-11-metrics.json）；
 *  3. 归档先行/失败跳过、最终集合不变、`audit_log` 不清理（可执行测试）；
 *  4. 新装 `auto_vacuum=INCREMENTAL`、有界 `incremental_vacuum`、维护态 VACUUM
 *     空间预检/告警；
 *  5. live/物理分开报告（不得用 live 宣称物理达标）；
 *  6. 契约计数 87/35/11/0014 不变；
 *  7. 证据文档/指标/脚本存在；
 *  8. 生成物 `protocol check` diff=0（需 PATH 中有 cargo）。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
const failures = [];

function check(name, condition, detail = "") {
  checks.push(name);
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function mustContain(rel, needles) {
  const text = read(rel);
  for (const needle of needles) {
    check(`${rel} 含 ${needle}`, text.includes(needle));
  }
}

function run(label, command, args) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    if (process.platform === "win32" && command.endsWith(".cmd")) {
      execFileSync(
        process.env.ComSpec ?? "cmd.exe",
        ["/d", "/s", "/c", [command, ...args].join(" ")],
        { cwd: root, stdio: "inherit", env: process.env },
      );
    } else {
      execFileSync(command, args, { cwd: root, stdio: "inherit", env: process.env });
    }
    checks.push(label);
  } catch (error) {
    failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";

// ---------- 1) 产品选批 ----------
mustContain("crates/aether-store/src/archive.rs", [
  "RETENTION_SELECT_SQL",
  "ORDER BY type ASC, ts ASC",
  "explain_retention_batch",
  "idx_events_type_ts",
]);
check(
  "archive.rs 不再使用 ORDER BY rowid",
  !read("crates/aether-store/src/archive.rs").includes("ORDER BY rowid"),
);
mustContain("crates/aether-app/src/retention.rs", [
  "RETENTION_DELETE_BATCH_DEFAULT",
  "RETENTION_DELETE_BATCH_TIERS",
  "RETENTION_ARCHIVE_BATCH_DEFAULT",
  "RETENTION_INCREMENTAL_VACUUM_PAGES",
  "StoreCommand::IncrementalVacuum",
  "run_maintenance_vacuum",
  "physical_bytes",
  "live_bytes",
  "physical_warning",
]);
mustContain("crates/aether-store/src/ops.rs", [
  "IncrementalVacuum",
  "VacuumAll",
  "incremental_vacuum",
]);
mustContain("crates/aether-store/src/write_queue.rs", [
  "StoreCommand::IncrementalVacuum",
  "StoreCommand::VacuumAll",
  "execute_maintenance",
]);
mustContain("crates/aether-store/src/pragma.rs", ["auto_vacuum", "INCREMENTAL"]);
mustContain("crates/aether-app/src/ipc/dispatch.rs", [
  "BACKUP_MAINTENANCE_ALLOWED_COMMANDS",
  "\"vacuum\"",
]);
mustContain("crates/aether-app/tests/ipc_validation.rs", [
  "vacuum_maintenance_allows_only_health_and_app_exit",
]);

const retentionSource = read("crates/aether-app/src/retention.rs");
const defaultTier = retentionSource.match(
  /RETENTION_DELETE_BATCH_DEFAULT: usize = ([\d_]+);/,
);
const defaultTierValue = defaultTier
  ? Number(defaultTier[1].replaceAll("_", ""))
  : null;
check("删除默认档位可解析", defaultTierValue !== null, `${defaultTierValue}`);
check(
  "删除默认档位属于 500/1000/2000/5000",
  [500, 1000, 2000, 5000].includes(defaultTierValue),
  `${defaultTierValue}`,
);

// ---------- 2) 证据与指标 ----------
for (const rel of [
  "docs/M10-11-证据.md",
  "docs/M10-11-metrics.json",
  "scripts/test/m10-11/verify-m10-11.mjs",
  "scripts/test/m10-11/run-bench.mjs",
  "crates/aether-store/tests/m10_11_retention_shrink.rs",
  "crates/aether-store/tests/m10_11_bench.rs",
  "crates/aether-app/tests/m10_11_retention.rs",
  "crates/aether-app/tests/m10_11_outlier_probe.rs",
]) {
  check(`证据存在 ${rel}`, existsSync(path.join(root, rel)));
}

const metrics = readJson("docs/M10-11-metrics.json");
const tiers = metrics.delete_matrix?.tiers ?? [];
check(
  "矩阵含 4 档 500/1000/2000/5000",
  [500, 1000, 2000, 5000].every((tier) =>
    tiers.some((entry) => entry.tier === tier),
  ),
  JSON.stringify(tiers.map((entry) => entry.tier)),
);
for (const entry of tiers) {
  check(
    `档位 ${entry.tier} 记录 P50/P95/max`,
    typeof entry.delete?.p50_ms === "number" &&
      typeof entry.delete?.p95_ms === "number" &&
      typeof entry.delete?.max_ms === "number",
    JSON.stringify(entry.delete),
  );
  check(
    `档位 ${entry.tier} EXPLAIN 无 TEMP B-TREE 且命中 idx_events_type_ts`,
    Array.isArray(entry.explain) &&
      entry.explain.some((line) => line.includes("idx_events_type_ts")) &&
      !entry.explain.some((line) => line.toUpperCase().includes("TEMP B-TREE")),
    JSON.stringify(entry.explain),
  );
}
check(
  "选批上限 5000 / 归档块 1000",
  metrics.delete_matrix?.selection_limit === 5000 &&
    metrics.delete_matrix?.archive_batch === 1000,
  `${metrics.delete_matrix?.selection_limit}/${metrics.delete_matrix?.archive_batch}`,
);
const chosen = tiers.find((entry) => entry.tier === metrics.delete_matrix?.chosen_tier);
check(
  `选定档位删除 P95 <500ms（${metrics.delete_matrix?.chosen_tier}）`,
  typeof chosen?.delete?.p95_ms === "number" && chosen.delete.p95_ms < 500,
  `${chosen?.delete?.p95_ms}`,
);
check(
  "选定档位持续清理无 >500ms 批",
  chosen?.delete?.slow_count === 0,
  `${chosen?.delete?.slow_count}`,
);
const compliantTiers = tiers
  .filter((entry) => entry.delete?.p95_ms < 500)
  .map((entry) => entry.tier)
  .sort((left, right) => left - right);
check(
  "选定档位 = 最大合规档",
  compliantTiers.at(-1) === metrics.delete_matrix?.chosen_tier,
  `compliant=${JSON.stringify(compliantTiers)}`,
);
check(
  "持续矩阵记录分辨率/纠正说明",
  typeof metrics.long_tail_probe?.resolution === "string" &&
    metrics.long_tail_probe.resolution.includes("500"),
  JSON.stringify(metrics.long_tail_probe?.resolution),
);
check(
  `矩阵选定档位 == 产品默认档位（${defaultTierValue}）`,
  metrics.delete_matrix?.chosen_tier === defaultTierValue,
  `${metrics.delete_matrix?.chosen_tier}`,
);
check(
  "无仅取平均的伪证据（矩阵含 p50/p95/max）",
  tiers.every(
    (entry) =>
      typeof entry.delete?.p50_ms === "number" &&
      typeof entry.delete?.p95_ms === "number" &&
      typeof entry.delete?.max_ms === "number",
  ),
);
const vacuum = metrics.incremental_vacuum ?? {};
check(
  "incremental_vacuum 物理收缩",
  vacuum.physical_after < vacuum.physical_before,
  JSON.stringify(vacuum),
);
check("新装 auto_vacuum=INCREMENTAL（主库+8 分片）", vacuum.auto_vacuum === 2);
check(
  "有界 incremental_vacuum 页数上限记录",
  typeof vacuum.pages === "number" && vacuum.pages > 0,
  `${vacuum.pages}`,
);
const livePhysical = metrics.live_physical ?? {};
check(
  "live/物理分开报告",
  typeof livePhysical.live_bytes === "number" &&
    typeof livePhysical.physical_bytes === "number" &&
    livePhysical.live_bytes !== livePhysical.physical_bytes,
  JSON.stringify(livePhysical),
);
check(
  "5.22GiB 物理 >5GiB 告警（不得用 live 宣称物理达标）",
  livePhysical.physical_over_5gib_warning === true &&
    livePhysical.physical_bytes > 5 * 1024 * 1024 * 1024,
  JSON.stringify(livePhysical),
);
const fullHistory = metrics.full_history ?? {};
check(
  "全量历史清理 ≤4h",
  typeof fullHistory.elapsed_ms === "number" &&
    fullHistory.elapsed_ms <= 4 * 60 * 60 * 1000,
  JSON.stringify(fullHistory),
);
const daily = metrics.daily_incremental ?? {};
check(
  "日常增量 ≤5min",
  typeof daily.elapsed_ms === "number" && daily.elapsed_ms <= 5 * 60 * 1000,
  JSON.stringify(daily),
);
const maintenance = metrics.maintenance_vacuum ?? {};
check(
  "维护态 VACUUM 空间预检 ≥ db+wal ×2",
  typeof maintenance.required_space_bytes === "number" &&
    maintenance.required_space_bytes >= maintenance.physical_before * 2,
  JSON.stringify(maintenance),
);
check(
  "维护态 VACUUM 记录停写窗口/耗时",
  typeof maintenance.stop_write_ms === "number" &&
    typeof maintenance.took_ms === "number",
  JSON.stringify(maintenance),
);

// ---------- 3) 契约计数 ----------
const manifest = readJson("packages/protocol/src/command-manifest.json");
const total = manifest.commands?.length ?? manifest.total ?? manifest.counts?.total;
check("命令 87", total === 87, `实际 ${total}`);
const errorCodes = readJson("packages/protocol/src/error_codes.json");
check(
  "IpcErrorCode 32",
  Array.isArray(errorCodes.ipc) && errorCodes.ipc.length === 32,
  `实际 ${errorCodes.ipc?.length}`,
);
check(
  "业务码 35",
  Array.isArray(errorCodes.business) && errorCodes.business.length === 35,
  `实际 ${errorCodes.business?.length}`,
);
const settings = read("crates/aether-app/src/ipc/dto.rs").match(
  /pub const SETTINGS_KEY_ALLOWLIST: &\[&str\] = &\[([\s\S]*?)\n\];/,
);
const settingsCount = settings ? [...settings[1].matchAll(/"([^"]+)"/g)].length : 0;
check("设置键 11", settingsCount === 11, `实际 ${settingsCount}`);
const migrations = readdirSync(path.join(root, "migrations"))
  .filter((file) => /^\d{4}_.*\.sql$/.test(file))
  .sort();
check("迁移计数 14", migrations.length === 14, `实际 ${migrations.length}`);
check(
  "0014 存在",
  migrations.includes("0014_session_shard.sql"),
  migrations.join(","),
);

// ---------- 4) 可执行测试 ----------
run("store 保留清理/收缩（m10_11_retention_shrink）", "cargo", [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m10_11_retention_shrink",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("app 保留清理/维护态 VACUUM（m10_11_retention）", "cargo", [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m10_11_retention",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("app 保留策略单测（retention::tests）", "cargo", [
  "test",
  "-p",
  "aether-app",
  "--lib",
  "retention::tests",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("app 手动 VACUUM 维护态严格 allowlist", "cargo", [
  "test",
  "-p",
  "aether-app",
  "--test",
  "ipc_validation",
  "vacuum_maintenance_allows_only_health_and_app_exit",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);

// ---------- 5) 生成物 ----------
run("protocol check", pnpm, ["--filter", "@aether/protocol", "check"]);
run("verify:error-codes", pnpm, ["verify:error-codes"]);

console.log("\n===== verify-m10-11 =====");
for (const name of checks) console.log(`PASS  ${name}`);
if (failures.length > 0) {
  console.error(`\n失败 ${failures.length} 项：`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log("\nAETHER_M10_11 PASS");
