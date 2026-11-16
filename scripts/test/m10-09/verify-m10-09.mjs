#!/usr/bin/env node
/**
 * `pnpm verify:m10-09` —— 事件分库实施（ADR-039 v1.0）可复跑检查。
 *
 * 检查面：
 *  1. 迁移 0014 存在且包含 `sessions.shard_id`/`deletion_pending` + 2 索引；
 *  2. 契约计数：命令 87 / 业务码 35 / 设置键 11 / 迁移 14；
 *  3. generation 布局/分片路由/tombstone/备份恢复 store 验收（m10_09_sharding）；
 *  4. 导入多库 staging 与既有契约（m10_04_import）；
 *  5. 加密整套备份与安全模式维护例外（m5_03/m9_13）；
 *  6. 证据文档/指标 JSON 存在；
 *  7. 生成物：`protocol check` diff=0（需 PATH 中有 cargo）。
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

// ---------- 1) 迁移 0014 ----------
const migration = "migrations/0014_session_shard.sql";
check("0014 迁移存在", existsSync(path.join(root, migration)));
mustContain(migration, [
  "ADD COLUMN shard_id INTEGER",
  "ADD COLUMN deletion_pending INTEGER NOT NULL DEFAULT 0",
  "CHECK (deletion_pending IN (0,1))",
  "idx_sessions_shard_id",
  "idx_sessions_deletion_pending",
]);
const migrations = readdirSync(path.join(root, "migrations"))
  .filter((file) => /^\d{4}_.*\.sql$/.test(file))
  .sort();
check("迁移计数 14", migrations.length === 14, `实际 ${migrations.length}`);

// ---------- 2) 契约计数 ----------
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

// ---------- 3) 关键源码/契约存在 ----------
mustContain("crates/aether-store/src/layout.rs", [
  "pub fn shard_for",
  "pub fn initialize",
  "pub fn assert_invariants",
  "pub fn write_current",
  "pub fn commit_staged_generation",
  "pub fn cleanup_old_generations",
]);
mustContain("crates/aether-store/src/import.rs", [
  "pub fn build_import_generation",
  "staging generation",
]);
mustContain("crates/aether-store/src/generation_backup.rs", [
  "pub fn create_backup_set",
  "pub fn validate_backup_set",
  "pub fn stage_restore_with",
  "pub fn restore_space_required",
]);
mustContain("crates/aether-app/src/ipc/dispatch.rs", [
  "BACKUP_MAINTENANCE_ALLOWED_COMMANDS",
  "health",
  "app_exit",
]);
mustContain("crates/aether-app/src/core_health.rs", [
  "pub struct StorageCheckSnapshot",
  "pub maintenance: Option<MaintenanceSnapshot>",
]);
mustContain("crates/aether-app/src/ipc/path.rs", ["pub fn validate_external_backup_set"]);
mustContain("crates/aether-store/src/write_queue.rs", [
  "pub fn open_generation",
  "delete_session_tombstone",
  "commit_events_batch",
]);

// ---------- 4) 证据 ----------
for (const rel of [
  "docs/M10-09-证据.md",
  "docs/M10-09-metrics.json",
  "docs/M10-09-Windows-CURRENT-原子替换说明.md",
  "scripts/test/m10-09/verify-m10-09.mjs",
  "scripts/test/m10-09/build-generation.mjs",
  "scripts/test/m10-09/run-bench.mjs",
  "scripts/test/m10-09/merge-bench.mjs",
  "crates/aether-store/tests/m10_09_bench.rs",
]) {
  check(`证据存在 ${rel}`, existsSync(path.join(root, rel)));
}
const metrics = readJson("docs/M10-09-metrics.json");
check("metrics 含 write_p95_ms", typeof metrics.write_p95_ms === "number");
check("metrics 含 events_page_p95_ms", typeof metrics.events_page_p95_ms === "number");
check("metrics 含 shard_count=8", metrics.shard_count === 8);
check("metrics 含失败注入计数", metrics.fault_injections !== undefined);
check(
  "metrics 含标准 600s 复测结果",
  typeof metrics.standard_600s === "object" &&
    metrics.standard_600s !== null &&
    metrics.standard_600s.failures === 0,
);

// ---------- 5) 可执行测试 ----------
run("store 分库验收（m10_09_sharding）", "cargo", [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m10_09_sharding",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("store 导入多库 staging（m10_04_import）", "cargo", [
  "test",
  "-p",
  "aether-store",
  "--test",
  "m10_04_import",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("app 加密整套备份（m5_03）", "cargo", [
  "test",
  "-p",
  "aether-app",
  "--test",
  "m5_03_encrypted_backup",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("daemon 安全模式整套恢复（m9_13）", "cargo", [
  "test",
  "-p",
  "aether-daemon",
  "--test",
  "m9_13_safe_mode_restore",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);
run("daemon 导入重启切换（m10_04）", "cargo", [
  "test",
  "-p",
  "aether-daemon",
  "--test",
  "m10_04_import",
  "-j",
  "1",
  "--",
  "--test-threads=1",
]);

// ---------- 6) 生成物 ----------
run("protocol check", pnpm, ["--filter", "@aether/protocol", "check"]);
run("verify:error-codes", pnpm, ["verify:error-codes"]);
run("verify:m9-01-baseline", pnpm, ["verify:m9-01-baseline"]);
run("verify:m1-03", pnpm, ["verify:m1-03"]);

console.log("\n===== verify-m10-09 =====");
for (const name of checks) console.log(`PASS  ${name}`);
if (failures.length > 0) {
  console.error(`\n失败 ${failures.length} 项：`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log("\nAETHER_M10_09 PASS");
