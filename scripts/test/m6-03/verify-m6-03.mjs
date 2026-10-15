/**
 * M6-03 验证脚本：完整审计查询与导出（设计 D9/SE-03、ADR-023 v0.3、ADR-018；
 * P1 计划 M6-03 DoD1–DoD6）。
 *
 * 覆盖：
 *  - DoD1：记录完备性——`m6_03_audit` 集成（真实 Mock：协议上报终态工具事件 ↔
 *    `tool.call` 审计行 1:1；tool_call_id + result 集合相等）；
 *  - DoD2：查询过滤/分页/稳定排序（tauri 集成 + store schema 测试；P95 基准
 *    `m6_03_audit_bench`，release + ignored：1e6 行 P95 ≤300ms）；
 *  - DoD3：导出（JSONL + manifest 校验、过滤、脱敏 0 命中、外部路径护栏、
 *    失败不留半成品）；
 *  - DoD4：不可改——源码静态扫描（`crates/` 无 UPDATE/DELETE/REPLACE/DROP/ALTER
 *    `audit_log`；`StoreCommand` 仅 `InsertAudit`）+ `m3_07_audit_schema` 复跑；
 *  - DoD5：保留联动——`m6_03_audit::retention_policy_never_deletes_audit_rows`
 *    + `m5_04_retention` 复跑；
 *  - DoD6：IPC 命令登记与参数校验矩阵（`ipc_validation`；bindings 生成物校验 T14）；
 *  - UI：审计页锚点与行为（vitest 定向 + `tsc --noEmit`）。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 * Cargo / pnpm / Node 经 scripts/test/lib/exec.mjs 解析。
 */
import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m6-03");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(tmpDir, `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 静态：附录 C / 迁移 0005 / bindings / UI 锚点 / 不可改扫描 / ADR-023 v0.3 =====

function checkStatic() {
  const doc = read("设计文档.md");
  const appendixStart = doc.indexOf("## 附录 C");
  const codeStart = doc.indexOf("```sql", appendixStart);
  const codeEnd = doc.indexOf("```", codeStart + 6);
  const appendixC = doc
    .slice(codeStart + 6, codeEnd)
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, "").replace(/\s+/g, " ").trim())
    .join("\n");
  for (const token of [
    "CREATE INDEX idx_audit_session_ts ON audit_log(session_id, ts, id)",
    "CREATE INDEX idx_audit_actor_ts ON audit_log(actor, ts, id)",
    "CREATE INDEX idx_audit_action_ts ON audit_log(action, ts, id)",
  ]) {
    if (!appendixC.includes(token)) {
      throw new Error(`附录 C 缺少审计索引终态 DDL：${token}`);
    }
  }

  const migration = read("migrations/0005_audit_indexes.sql");
  for (const token of [
    "idx_audit_session_ts",
    "idx_audit_actor_ts",
    "idx_audit_action_ts",
    "(session_id, ts, id)",
    "(actor, ts, id)",
    "(action, ts, id)",
  ]) {
    if (!migration.includes(token)) {
      throw new Error(`迁移 0005 缺少：${token}`);
    }
  }
  if (!read("crates/aether-store/src/migration.rs").includes("0005_audit_indexes.sql")) {
    throw new Error("EMBEDDED_MIGRATIONS 未内嵌 0005");
  }

  // ADR-023 v0.3（§6-3 索引/分页设计修订在案）。
  const adr = read("docs/adr/ADR-023-full-audit-scope-and-format.md");
  for (const token of ["v0.3", "has_more", "COUNT(*)", "(session_id, ts, id)"]) {
    if (!adr.includes(token)) {
      throw new Error(`ADR-023 缺少 v0.3 修订要素：${token}`);
    }
  }

  // 生成物（T14 口径：静态核验关键契约）。
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const token of [
    '__TAURI_INVOKE("audit_query"',
    '__TAURI_INVOKE("audit_export"',
    "export type AuditQueryRequest",
    "export type AuditExportRequest",
  ]) {
    if (!bindings.includes(token)) {
      throw new Error(`bindings.ts 缺少契约 token：${token}`);
    }
  }

  // UI 锚点（ADR-023 §2.1-4 / UI-UX §7.3）。
  const page = read("apps/desktop/src/AuditPage.tsx");
  for (const token of [
    "audit-page",
    "audit-filter",
    "audit-export",
    "audit-export-result",
    "audit-error",
  ]) {
    if (!page.includes(token)) {
      throw new Error(`AuditPage 缺少锚点 token：${token}`);
    }
  }

  // DoD4：应用层 audit_log 无 UPDATE/DELETE/REPLACE/DROP/ALTER 路径 + StoreCommand 白名单。
  const forbidden = [
    [/\bUPDATE\s+audit_log\b/i, "UPDATE audit_log"],
    [/\bDELETE\s+FROM\s+audit_log\b/i, "DELETE FROM audit_log"],
    [/\bINSERT\s+OR\s+REPLACE\s+INTO\s+audit_log\b/i, "INSERT OR REPLACE INTO audit_log"],
    [/\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?audit_log\b/i, "DROP TABLE audit_log"],
    [/\bALTER\s+TABLE\s+audit_log\b/i, "ALTER TABLE audit_log"],
  ];
  const rustSources = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name === "target" || entry.name === ".git") continue;
        walk(full);
      } else if (entry.isFile() && entry.name.endsWith(".rs")) {
        rustSources.push(full);
      }
    }
  };
  walk(path.join(repoRoot, "crates"));
  for (const file of rustSources) {
    const source = readFileSync(file, "utf8");
    for (const [pattern, label] of forbidden) {
      if (pattern.test(source)) {
        throw new Error(`${path.relative(repoRoot, file)} 命中禁止语句：${label}`);
      }
    }
  }
  const ops = read("crates/aether-store/src/ops.rs");
  const auditCommands = [...ops.matchAll(/(InsertAudit|UpdateAudit|DeleteAudit|ReplaceAudit|UpsertAudit)/g)].map(
    (match) => match[1],
  );
  const uniqueCommands = [...new Set(auditCommands)];
  if (uniqueCommands.length === 0 || uniqueCommands.some((name) => name !== "InsertAudit")) {
    throw new Error(`StoreCommand 审计路径白名单违例：${uniqueCommands.join(",")}`);
  }
  console.log(
    "[static] 附录 C/迁移 0005/ADR-023 v0.3/bindings/UI 锚点/不可改扫描（audit_log 仅 INSERT）通过",
  );
}

try {
  checkStatic();
  record("静态：附录 C/迁移 0005/ADR-023 v0.3/bindings/UI 锚点/不可改扫描", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：附录 C/迁移 0005/ADR-023 v0.3/bindings/UI 锚点/不可改扫描", 1);
}

// ===== DoD1/2/3/5：Rust 集成（Mock 适配器 E2E + 查询/导出/保留）=====

record(
  "编译 Mock 适配器单文件（bun build --compile；DoD1 E2E 宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

const integrationEnv = {
  AETHER_MOCK_ADAPTER: mockAdapter,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
  AETHER_M6_03_EVIDENCE_DIR: evidenceDir,
};
record(
  "DoD1/2/3/5：m6_03_audit（工具调用 100% 落审计 + 查询/导出/保留 + 静态不可改断言）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m6_03_audit", "--", "--nocapture"], {
    cwd: repoRoot,
    env: integrationEnv,
  }),
);
record(
  "DoD1：迁移 0005 索引/幂等 + audit_query 行为（store 集成）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m6_03_audit_schema"], { cwd: repoRoot }),
);
record(
  "DoD1/4：附录 C ↔ 迁移集 ↔ 真实库双向比对（verify:m1-03；EXPECTED_EXPLICIT_INDEXES 9）",
  run(process.execPath, ["scripts/test/m1-03/verify-m1-03.mjs"], { cwd: repoRoot }),
);
record(
  "DoD4：audit_log schema/触发器回归（m3_07_audit_schema）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_07_audit_schema"], { cwd: repoRoot }),
);
record(
  "DoD5：保留策略回归（m5_04_retention；审计不被触碰）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m5_04_retention"], { cwd: repoRoot }),
);

// ===== DoD2：1e6 行 P95 基准（release + ignored；ADR-023 阈值 300ms 不变）=====

record(
  "DoD2 基准：1e6 行已过滤分页查询 P95 ≤300ms（release + ignored）",
  run(
    cargo,
    [
      "test",
      "--release",
      "-p",
      "aether-tauri",
      "--test",
      "m6_03_audit_bench",
      "--",
      "--ignored",
      "--nocapture",
    ],
    { cwd: repoRoot, env: { AETHER_M6_03_EVIDENCE_DIR: evidenceDir } },
  ),
);

// ===== DoD6：IPC 矩阵 + T14 生成物 + UI =====

record(
  "DoD6：IPC 参数校验矩阵（audit_query/audit_export 畸形样本全拒绝；ipc_validation）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);
record(
  "DoD6/T14：bindings 生成物校验（tauri-specta 重新生成 + git diff --exit-code）",
  run(process.execPath, ["scripts/ci/bindings.mjs", "check"], { cwd: repoRoot }),
);
record(
  "UI：审计页（锚点/过滤/分页/导出/错误码）vitest 定向",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m6_03_audit.test.tsx",
  ]),
);
record(
  "UI：前端类型检查（tsc --noEmit）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);

// ===== 证据归档 =====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  console.log(`[m6-03] 证据目录：${evidenceDir}（${files.length} 份 JSON）`);
  record(`证据归档：${files.length} 份（DoD1/2/3/5 运行期证据）`, files.length >= 4 ? 0 : 1);
}

process.exit(summarize("verify-m6-03", checks));
