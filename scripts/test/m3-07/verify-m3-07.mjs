/**
 * M3-07 验证入口：审计最小集（设计 D9 / SE-03；ADR-003/ADR-004）。
 *
 * 覆盖 DoD：
 *   1) 注入动作与审计条数一一对应（脚本比对）——
 *      ① 会话生命周期 `m3_07_audit::session_lifecycle_audits_are_one_to_one`
 *         （create/send/dispose 6 条审计与 `session.*` 事件逐条对应）；
 *      ② 权限决议 `m3_07_audit::permission_decision_audits_are_one_to_one`
 *         （策略直决/ask 决议/超时共 6 条审计）；
 *      ③ 适配器状态变化 `m3_07_adapter_audit`（启动失败 2 条 + 准入拒绝 2 条）；
 *   2) 应用层无 UPDATE/DELETE 路径（静态审计）——本脚本扫描 `crates/` 全部 Rust
 *      源文件，禁止 `UPDATE audit_log` / `DELETE FROM audit_log` / `INSERT OR REPLACE`
 *      / `DROP`/`ALTER TABLE audit_log`，并断言 `StoreCommand` 仅有 `InsertAudit`；
 *   3) 字段 actor/resource/result/ts 齐备（schema 断言）——
 *      `aether-store --test m3_07_audit_schema`（PRAGMA 列/类型/NOT NULL + 无触发器）
 *      + 集成侧逐行字段非空断言（证据 JSON 归档）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；证据归档到 scripts/test/.tmp/m3-07。
 */
import { readdirSync, readFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-07");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);
const evidenceEnv = { AETHER_M3_07_EVIDENCE_DIR: evidenceDir };

mkdirSync(evidenceDir, { recursive: true });

// ===== 1. DoD3：`audit_log` 最小集字段 schema 断言 =====

record(
  "aether-store --test m3_07_audit_schema（actor/resource/result/ts 列/类型/NOT NULL + 无触发器 + 回读）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_07_audit_schema"], {
    cwd: repoRoot,
  }),
);

// ===== 2. DoD1/DoD3：会话生命周期 + 权限决议 1:1（aether-control）=====

record(
  "m3_07_audit（会话生命周期 1:1 + 权限决议 1:1 + 字段齐备；证据 JSON 归档）",
  run(
    cargo,
    ["test", "-p", "aether-control", "--test", "m3_07_audit", "--", "--nocapture"],
    {
      cwd: repoRoot,
      env: evidenceEnv,
    },
  ),
);

// ===== 3. DoD1/DoD3：适配器状态变化 1:1（aether-tauri）+ 延迟观察者 =====

record(
  "m3_07_adapter_audit（启动失败 1:1 + 准入拒绝 1:1 + 延迟观察者丢弃注入前回调）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-tauri",
      "--test",
      "m3_07_adapter_audit",
      "--",
      "--nocapture",
    ],
    {
      cwd: repoRoot,
      env: evidenceEnv,
    },
  ),
);

// ===== 4. 回归：既有权限/生命周期语义不受审计写入影响 =====

record(
  "m2_01_lifecycle（回归：状态机/run 串行/幂等/超时；审计写入不改变语义）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"], {
    cwd: repoRoot,
  }),
);
record(
  "m2_03_permission（回归：策略矩阵/T7/审批/超时审计）",
  run(cargo, ["test", "-p", "aether-control", "--test", "m2_03_permission"], {
    cwd: repoRoot,
  }),
);

// ===== 5. DoD2：应用层无 UPDATE/DELETE 路径（静态审计）=====

const forbidden = [
  [/\bUPDATE\s+audit_log\b/i, "UPDATE audit_log"],
  [/\bDELETE\s+FROM\s+audit_log\b/i, "DELETE FROM audit_log"],
  [/\bINSERT\s+OR\s+REPLACE\s+INTO\s+audit_log\b/i, "INSERT OR REPLACE INTO audit_log"],
  [/\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?audit_log\b/i, "DROP TABLE audit_log"],
  [/\bALTER\s+TABLE\s+audit_log\b/i, "ALTER TABLE audit_log"],
];

function rustSources(dir) {
  const found = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "target" || entry.name === ".git") continue;
      found.push(...rustSources(full));
    } else if (entry.isFile() && entry.name.endsWith(".rs")) {
      found.push(full);
    }
  }
  return found;
}

{
  const files = rustSources(path.join(repoRoot, "crates"));
  const migrations = readdirSync(path.join(repoRoot, "migrations"))
    .filter((name) => name.endsWith(".sql"))
    .map((name) => path.join(repoRoot, "migrations", name));
  const hits = [];
  for (const file of [...files, ...migrations]) {
    const text = readFileSync(file, "utf8");
    for (const [pattern, label] of forbidden) {
      if (pattern.test(text)) {
        hits.push(`${path.relative(repoRoot, file)}: ${label}`);
      }
    }
  }
  const ops = readFileSync(path.join(repoRoot, "crates", "aether-store", "src", "ops.rs"), "utf8");
  const insertPresent = /INSERT INTO audit_log\b/.test(ops);
  const updateVariant = /\b(?:UpdateAudit|DeleteAudit)\b/.test(ops);
  if (!insertPresent) hits.push("ops.rs 缺少 INSERT INTO audit_log（追加路径缺失）");
  if (updateVariant) hits.push("StoreCommand 出现 UpdateAudit/DeleteAudit 变体");
  if (hits.length > 0) console.error(hits.join("\n"));
  writeFileSync(
    path.join(evidenceDir, "dod2_append_only.json"),
    `${JSON.stringify(
      {
        task: "M3-07 DoD2 应用层无 UPDATE/DELETE 路径（静态审计）",
        scanned_files: files.length,
        scanned_migrations: migrations.length,
        forbidden_patterns: forbidden.map(([, label]) => label),
        hits,
        insert_path_present: insertPresent,
        store_command_has_no_update_delete_audit: !updateVariant,
        pass: hits.length === 0,
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
  record(
    `静态审计：${files.length} 个 Rust 源文件 + ${migrations.length} 个迁移文件无 audit_log UPDATE/DELETE/REPLACE（StoreCommand 仅 InsertAudit）`,
    hits.length === 0 ? 0 : 1,
  );
}

// ===== 6. DoD1 脚本比对：三类证据 `actual == expected` 且 `one_to_one` =====

{
  const evidenceFiles = [
    "dod1_session_lifecycle.json",
    "dod1_permission_decisions.json",
    "dod1_adapter_start_failed.json",
    "dod1_adapter_admission_rejected.json",
  ];
  const problems = [];
  for (const name of evidenceFiles) {
    const file = path.join(evidenceDir, name);
    if (!existsSync(file)) {
      problems.push(`${name}: 证据缺失`);
      continue;
    }
    let data;
    try {
      data = JSON.parse(readFileSync(file, "utf8"));
    } catch (error) {
      problems.push(`${name}: JSON 解析失败（${error.message}）`);
      continue;
    }
    if (JSON.stringify(data.actual) !== JSON.stringify(data.expected)) {
      problems.push(`${name}: actual != expected`);
    }
    if (data.one_to_one !== true) problems.push(`${name}: one_to_one != true`);
    if (!Array.isArray(data.rows) || data.rows.length !== (data.actual ?? []).length) {
      problems.push(`${name}: rows 条数与注入动作数不符`);
    }
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    `脚本比对：三类注入动作 ↔ 审计序列逐条一致（${evidenceFiles.length} 份证据 actual == expected）`,
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 7. 证据归档检查（DoD1 逐类 JSON 可出示）=====

{
  const required = [
    "dod1_session_lifecycle.json",
    "dod1_permission_decisions.json",
    "dod1_adapter_start_failed.json",
    "dod1_adapter_admission_rejected.json",
    "dod2_append_only.json",
  ];
  const missing = required.filter((name) => !existsSync(path.join(evidenceDir, name)));
  if (missing.length > 0) console.error(`缺少证据：${missing.join(", ")}`);
  writeFileSync(
    path.join(evidenceDir, "summary.json"),
    `${JSON.stringify(
      {
        task: "M3-07 审计最小集",
        stamp,
        evidence: required,
        categories: ["session_lifecycle", "permission_decisions", "adapter_status"],
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
  console.log(`[m3-07] 证据目录：${evidenceDir}`);
  record(
    "证据归档：三类 1:1 JSON + 静态审计 JSON 落盘（供 Gate 3 逐条出示）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-07", checks));
