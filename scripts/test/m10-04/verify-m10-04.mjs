#!/usr/bin/env node
/**
 * `pnpm verify:m10-04` —— 导入/导出完整（ADR-034）可复跑检查。
 *
 * 检查面：
 *  1. ADR-034 已接受；迁移 0001–0011 未修改、无 0012；
 *  2. 命令面 +4（83/79/4；UI 81；import 分组 4）；业务码 +7（31）；
 *  3. store 导入常量/函数（20GiB/2MiB/64KiB/1MiB/1000/staging/原子切换/预导入）；
 *  4. app 后端（异步 job/维护窗口/审计 import 与 export 动作、backup_restore 互斥）；
 *  5. CLI 导入 status/cancel/watch；UI 导入入口/状态/警告/取消/重启；
 *  6. 生成物检查（command-manifest/path-commands/error-codes/api-schema diff=0）；
 *  7. 证据文档 DoD-1～8。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
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

function git(args) {
  try {
    return execFileSync("git", ["-C", root, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return "";
  }
}

// 1) ADR / 迁移
const adr = read("docs/adr/ADR-034-m10-import-export-format-atomicity.md");
check("ADR-034 已接受", adr.includes("已接受") && adr.includes("v1.0"));
mustContain("docs/adr/ADR-034-m10-import-export-format-atomicity.md", [
  "staging DB",
  "`.pre-import`",
  "preserve",
  "remap",
  "client_msg_id",
  "import_too_large",
  "4（累计登记 78 → 82）",
]);
const migrationFiles = execFileSync(
  process.platform === "win32" ? "cmd" : "sh",
  process.platform === "win32"
    ? ["/c", "dir /b migrations"]
    : ["-c", "ls migrations"],
  { cwd: root, encoding: "utf8" },
)
  .split(/\r?\n/)
  .map((line) => line.trim())
  .filter(Boolean);
check("迁移仍为 0001–0011", migrationFiles.length === 11, migrationFiles.join(","));
check("无 0012 迁移", !migrationFiles.some((name) => name.startsWith("0012")));
check("已发布迁移未修改", git(["diff", "--name-only", "--", "migrations"]) === "");

// 2) 命令面/业务码
const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
for (const command of ["session_import", "archive_import", "import_status", "import_cancel"]) {
  check(`命令面含 ${command}`, dispatch.includes(`"${command}"`));
}
check("COMMANDS = 83", /COMMANDS\.len\(\),\s*83/.test(dispatch));
const manifest = readJson("packages/protocol/src/command-manifest.json");
check("manifest total=83", manifest.counts?.total === 83);
check("manifest daemon=79", manifest.counts?.daemon === 79);
check("manifest shell=4", manifest.counts?.shell === 4);
check("manifest ui=81", manifest.counts?.ui === 81);
check(
  "manifest import 分组=4",
  (manifest.groups ?? []).some((group) => group.group === "import" && group.count === 4),
);
const errors = readJson("packages/protocol/src/error_codes.json");
check("ipc 仍 32", errors.ipc?.length === 32);
check("business=31", errors.business?.length === 31, `实际 ${errors.business?.length}`);
for (const code of [
  "import_invalid_format",
  "import_checksum_mismatch",
  "import_conflict",
  "import_too_large",
  "import_cancelled",
  "import_job_not_found",
  "import_job_not_active",
]) {
  check(`业务码含 ${code}`, errors.business?.some((entry) => entry.code === code));
}

// 3) store 导入实现
mustContain("crates/aether-store/src/import.rs", [
  "IMPORT_FILE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024",
  "IMPORT_LINE_MAX_BYTES: usize = 2 * 1024 * 1024",
  "IMPORT_MANIFEST_LINE_MAX_BYTES: usize = 64 * 1024",
  "IMPORT_BUFFER_BYTES: usize = 1024 * 1024",
  "IMPORT_BATCH_ROWS: usize = 1000",
  "IMPORT_STALE_HOURS: i64 = 24",
  "PRE_IMPORT_INFIX",
  "VACUUM INTO",
  "execute_restore",
  "client_msg_id",
  "unknown_event_type",
  "import_checksum_mismatch",
  "AUDIT_ACTION_IMPORT_COMPLETED",
  "boot_apply_pending_import",
  "has_marker",
  "mark_staging",
]);
mustContain("crates/aether-store/src/lib.rs", ["pub mod import;", "pub use import::{"]);
mustContain("crates/aether-store/src/backup.rs", ["scene_infix", "plan_scene(&request.db_path"]);

// 4) app 后端/装配
mustContain("crates/aether-app/src/import_control.rs", [
  "pub struct ImportControlBackend",
  "maintenance_state",
  "session_import",
  "archive_import",
  "import_status",
  "import_cancel",
  "AUDIT_ACTION_IMPORT_STARTED",
  "AUDIT_ACTION_IMPORT_CANCELLED",
  "AUDIT_ACTION_IMPORT_FAILED",
  "export.started",
  "export.completed",
  "export.failed",
  "backup_restore",
  "ImportJobStatus::AwaitingRestart",
  "write_pending",
]);
mustContain("crates/aether-app/src/boot.rs", [
  "import_control::boot_apply_pending_import",
  "ImportControlBackend::new",
]);
mustContain("crates/aether-app/src/ipc/dispatch.rs", [
  "MAINTENANCE_ALLOWED_COMMANDS",
  "maintenance_reason",
]);
check(
  "import 命令走严格 DTO",
  read("crates/aether-app/src/ipc/handlers.rs").includes("request.canonical_source_path()"),
);

// 5) CLI/UI
const cli = read("crates/aether-daemon/src/cli.rs");
check("CLI 无 M10-04 未实现占位", !cli.includes("M10-04 实现前"));
for (const needle of ["--status", "--cancel", "--watch", "import_status", "import_cancel"]) {
  check(`CLI 含 ${needle}`, cli.includes(needle));
}
mustContain("apps/desktop/src/import.ts", [
  "ImportJobStatus",
  "startSession",
  "startArchive",
  "importIsTerminal",
  "importIsCancellable",
]);
mustContain("apps/desktop/src/BackupPage.tsx", [
  'data-testid="import"',
  'data-testid="import-archive"',
  'data-testid="import-dry-run"',
  'data-testid="import-status"',
  'data-testid="import-warnings"',
  'data-testid="import-cancel"',
  'data-testid="import-restart"',
]);
check(
  "UI 测试存在",
  existsSync(path.join(root, "apps/desktop/src/m10_04_import.test.tsx")),
);
check(
  "store 测试存在",
  existsSync(path.join(root, "crates/aether-store/tests/m10_04_import.rs")),
);
check(
  "daemon E2E 测试存在",
  existsSync(path.join(root, "crates/aether-daemon/tests/m10_04_import.rs")),
);
check(
  "≥1GB 复跑脚本存在",
  existsSync(path.join(root, "scripts/test/m10-04/big-import.mjs")),
);
check(
  "package.json 含 test:m10-04:big",
  Boolean(readJson("package.json").scripts?.["test:m10-04:big"]),
);

// 6) 生成物一致性（不运行协议生成器，避免未提交 diff；运行静态生成器 check）
for (const [script, expected] of [
  ["scripts/ci/command-manifest.mjs", "83/79/4"],
  ["scripts/ci/path-commands.mjs", "路径命令"],
  ["scripts/ci/error-codes.mjs", "ipc=32"],
]) {
  try {
    const output = execFileSync(process.execPath, [path.join(root, script), "check"], {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
    check(`${script} check`, output.includes("diff=0") || output.includes("通过") || output.includes(expected), output);
  } catch (error) {
    check(`${script} check`, false, String(error.stderr || error.message));
  }
}
try {
  const output = execFileSync(process.execPath, [path.join(root, "scripts/ci/api-schema.mjs"), "check"], {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
  check("api-schema check", output.includes("diff=0"), output);
} catch (error) {
  check("api-schema check", false, String(error.stderr || error.message));
}

// 7) 证据
const evidencePath = "docs/M10-04-证据.md";
check("证据文档存在", existsSync(path.join(root, evidencePath)));
if (existsSync(path.join(root, evidencePath))) {
  const evidence = read(evidencePath);
  for (let index = 1; index <= 8; index += 1) {
    check(`证据含 DoD-${index}`, evidence.includes(`DoD-${index}`));
  }
  for (const needle of ["往返", "原子", "大库", "脱敏", "E2E", "import.completed"]) {
    check(`证据含 ${needle}`, evidence.includes(needle));
  }
  check(
    "证据不含真实密钥样本",
    !/sk-[A-Za-z0-9_-]{20,}|api[_-]?key\s*[:=]\s*['"][^'"]{12,}/i.test(evidence),
  );
}

const passed = checks.filter(Boolean).length;
for (const failure of failures) console.error(`[m10-04] FAIL ${failure}`);
if (failures.length > 0) {
  console.error(`[m10-04] ${failures.length} 项失败（${checks.length - failures.length}/${checks.length} 通过）`);
  process.exit(1);
}
console.log(
  `[m10-04] OK：${checks.length}/${checks.length} 项通过（ADR-034；83 命令 / 31 业务码；导入 staging/原子切换/脱敏）`,
);
