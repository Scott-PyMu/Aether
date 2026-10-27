/**
 * M5-05 验证入口：归档导出与会话 JSONL（设计 D13/D10/D11、ADR-021、ADR-018）。
 *
 * 覆盖 DoD：
 *   1) Schema 一致性（逐字段映射/双序列）—— `m5_05_export`
 *      `session_export_field_mapping_and_sequences`；
 *   2) 完整性（行数/哈希/中断不留半成品）—— store 单测（原子写出/篡改检出/错误注入）
 *      + `kill_during_export_leaves_no_half_artifact`；≥1GB 大库抽样
 *      `m5_05_export_big`（`AETHER_M5_05_BIG=1` 显式启用，禁止静默跳过）；
 *   3) 脱敏（0 命中 + `api_key_ref` 原样）—— `export_redacts_secrets_and_keeps_api_key_refs`
 *      + `verify-archive.mjs` 独立扫描；
 *   4) 外部路径/空间护栏（M3-04 同口径）—— `external_target_and_space_guards`；
 *   5) 性能与内存（基准）—— `m5_05_export_bench`（release + ignored）；
 *   6) UI 面（E2E）+ 与 M5-04 归档链路同格式 —— 前端测试 +
 *      `export_artifact_is_consumable_by_retention_archive_chain`。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp/m5-05。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m5-05", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. store 单测（DoD1/2/3 底座：格式/哈希/原子写出/篡改检出/批选择） =====

record(
  "aether-store 单测（archive：manifest/sha256/原子写/错误注入/保留批白名单）",
  run(cargo, ["test", "-p", "aether-store", "--lib", "archive::tests"]),
);

// ===== 2. 集成测试（DoD1–DoD4/DoD6；证据归档） =====

record(
  "m5_05_export（DoD1 字段映射 / DoD2 kill 中断 / DoD3 脱敏 / DoD4 外部路径与空间护栏 / DoD6 链路同格式）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m5_05_export", "--", "--nocapture"],
    { cwd: repoRoot, env: { AETHER_M5_05_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== 3. DoD2：≥1GB 大库抽样（显式启用；禁止静默跳过） =====

if (process.env.AETHER_M5_05_BIG === "1") {
  record(
    "m5_05_export_big（release：≥1GB 库全量导出 + manifest 校验 + 首/末行抽样）",
    run(
      cargo,
      [
        "test",
        "--release",
        "-p",
        "aether-store",
        "--test",
        "m5_05_export_big",
        "--",
        "--ignored",
        "--nocapture",
      ],
      { cwd: repoRoot, env: { AETHER_M5_05_EVIDENCE_DIR: evidenceDir } },
    ),
  );
} else {
  checks.push({
    name: "m5_05_export_big（≥1GB 大库抽样）",
    skip: true,
    reason: "未设置 AETHER_M5_05_BIG=1（重 IO 可选任务；显式跳过，非静默）",
  });
}

// ===== 4. DoD5：导出期间写 P95/单事务/内存（release 基准） =====

record(
  "m5_05_export_bench（release：导出期间写事务 P95<50ms / 单事务<500ms / RSS 增量<256MB）",
  run(cargo, [
    "test",
    "--release",
    "-p",
    "aether-tauri",
    "--test",
    "m5_05_export_bench",
    "--",
    "--ignored",
    "--nocapture",
  ]),
);

// ===== 5. 前端（DoD6 UI 面 + 回归） =====

record(
  "pnpm --filter @aether/desktop test（一键归档三档联动 / 外部目录 / 会话导出 / 保留状态段 + 全量回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 6. 独立校验工具（verify-archive.mjs：行数/哈希/字段白名单/脱敏扫描） =====

{
  const tool = path.join(repoRoot, "scripts", "test", "m5-05", "verify-archive.mjs");
  const archives = [
    "dod1_session_archive.jsonl",
    "dod6_full_archive.jsonl",
    "dod6_retention_archive.jsonl",
  ]
    .map((name) => path.join(evidenceDir, name))
    .filter((file) => existsSync(file));
  const expectCount = 3;
  record(
    "verify-archive.mjs 独立校验（manifest/行数/sha256/字段白名单/密钥扫描 0 命中）",
    archives.length === expectCount
      ? run(node, [tool, ...archives])
      : 1,
  );
  if (archives.length !== expectCount) {
    console.error(
      `独立校验输入缺失：期望 ${expectCount} 份归档，实际 ${archives.length}（${archives.join(", ")}）`,
    );
  }
}

// ===== 7. 静态接线检查 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const storeArchive = read("crates/aether-store/src/archive.rs");
  const archiveControl = read("crates/aether-app/src/archive_control.rs");
  const retention = read("crates/aether-app/src/retention.rs");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
  const backupPage = read("apps/desktop/src/BackupPage.tsx");
  const topBar = read("apps/desktop/src/TopBar.tsx");
  const archiveTs = read("apps/desktop/src/archive.ts");

  const required = [
    [storeArchive, "ARCHIVE_FORMAT_VERSION", "格式版本登记"],
    [storeArchive, "ARCHIVE_PARTIAL_INFIX", "临时文件 + 原子 rename（中断不留半成品）"],
    [storeArchive, "verify_archive_file", "manifest 行数/sha256 校验"],
    [storeArchive, "RETENTION_EVENT_TYPES", "保留白名单常量"],
    [archiveControl, "AUDIT_ACTION_ARCHIVE_EXPORTED", "审计 archive.exported（ADR-023）"],
    [archiveControl, "AUDIT_ACTION_SESSION_EXPORTED", "审计 session.exported（ADR-023）"],
    [archiveControl, "required_space_bytes", "空间护栏（M3-04 同口径）"],
    [retention, "archive_retention_batch", "自动归档链路"],
    [retention, "DeleteEventsByIds", "先归档后删除（单写队列）"],
    [retention, "AUDIT_ACTION_RETENTION_ARMED", "阈值自动启停审计"],
    [commands, "fn archive_export", "命令面接线（archive_export）"],
    [commands, "fn session_export", "命令面接线（session_export）"],
    [lib, 'install_monitor("retention"', "保留任务退出序列停止"],
    [diagnostics, '"retention"', "诊断包 retention 段"],
    [diagnostics, '"export"', "诊断包 export 段"],
    [backupPage, 'data-testid="archive"', "UI-UX 锚点 archive（data-result）"],
    [backupPage, 'data-testid="retention-status"', "保留状态段（M5-04 UI）"],
    [topBar, 'data-testid="session-export"', "单会话导出锚点"],
    [archiveTs, "archive_export", "前端 IPC 封装（命令名）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 基线保护：迁移 0001–0003 未改（ADR-021：不改迁移）。
  const diff = spawnSync(
    "git",
    [
      "diff",
      "--name-only",
      "--",
      "migrations/0001_init.sql",
      "migrations/0002_unique_keys.sql",
      "migrations/0003_p0_ui_extensions.sql",
    ],
    { cwd: repoRoot, encoding: "utf8" },
  );
  const modified = (diff.stdout ?? "").split(/\r?\n/).filter(Boolean);
  if (modified.length > 0) problems.push(`已发布迁移被修改：${modified.join(", ")}`);

  // ADR 台账：ADR-021（保留/归档）+ ADR-018（命令登记）。
  const adr021 = path.join(repoRoot, "docs", "adr", "ADR-021-event-retention-and-archive.md");
  if (!existsSync(adr021)) problems.push("缺少 docs/adr/ADR-021-event-retention-and-archive.md");
  else {
    const text = readFileSync(adr021, "utf8");
    for (const token of ["session_export", "archive_export", "sha256", "auto-retention"]) {
      if (!text.includes(token)) problems.push(`ADR-021 缺少登记标记：${token}`);
    }
  }
  const adr018 = readFileSync(
    path.join(repoRoot, "docs", "adr", "ADR-018-p1-command-surface-batch.md"),
    "utf8",
  );
  for (const token of ["`session_export`", "`archive_export`", "52 → 63"]) {
    if (!adr018.includes(token)) problems.push(`ADR-018 缺少登记标记：${token}`);
  }

  // npm script 接线。
  const pkg = JSON.parse(read("package.json"));
  for (const script of ["verify:m5-05", "verify:m5-04"]) {
    if (!pkg.scripts?.[script]) problems.push(`package.json 缺少脚本：${script}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：格式/审计/空间护栏/诊断段/UI 锚点/迁移基线/ADR 登记在案", problems.length === 0 ? 0 : 1);
}

// ===== 8. 证据归档 =====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir)
    : [];
  const expectFiles = [
    "dod1_field_mapping.json",
    "dod1_session_archive.jsonl",
    "dod2_kill_interrupt.json",
    "dod3_redaction.json",
    "dod4_external_and_space_guards.json",
    "dod6_chain_consumable.json",
    "dod6_full_archive.jsonl",
    "dod6_retention_archive.jsonl",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify({ task: "M5-05", stamp, evidence_files: files, expect_files: expectFiles }, null, 2)}\n`,
    "utf8",
  );
  console.log(`[m5-05] 证据目录：${evidenceDir}`);
  record(
    "证据归档：DoD1–DoD6 逐用例 JSON + 独立校验归档输入（.jsonl）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m5-05", checks));
