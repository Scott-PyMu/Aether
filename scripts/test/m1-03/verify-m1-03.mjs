/**
 * M1-03 验证脚本：SQLite 存储层与迁移（D3、D12、评审#2、附录 C、ADR-004/ADR-005）。
 *
 * 覆盖 DoD：
 *   1) PRAGMA 断言：由 Rust 集成测试在真实库上逐项断言（journal_mode=wal / synchronous=normal /
 *      foreign_keys=on / busy_timeout=5000 / wal_autocheckpoint=1000 / journal_size_limit=67108864 /
 *      cache_size=-32000，另按 D3 含 temp_store=MEMORY）；
 *   2) 迁移幂等 + schema_migrations(version+checksum) + 篡改拒绝：Rust 集成测试；
 *   3) 附录 C 全部表/列/默认值/外键/索引逐项断言：
 *      - 静态：迁移集（0001 + 0002+）的**有效 schema** 与附录 C 逐项一致；
 *      - 实时：schema_dump 示例从真实库导出结构，与附录 C 解析结果逐项比对；
 *      - 含 `events UNIQUE(session_id, seq)` 与 `messages UNIQUE(session_id, client_msg_id)`
 *        的重复写入拒绝用例（Rust 集成测试）；
 *   4) 损坏库 → 安全模式（只读 + 备份/导出入口 + 拒绝写入）：Rust 集成测试；
 *   5) 既有库差异修复（ADR-004 决策 6）：0001 未修改；0002+ 增量迁移；迁移前重复键审计
 *      （scripts/test/m1-03/audit-duplicate-keys.mjs，只读，报告归档）。
 *
 * 说明：迁移文件内容经 `include_bytes!` 内嵌，sha256 即校验口径（D3 评审修订 #2）；
 * 本脚本额外输出磁盘文件 sha256 作为证据。
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const migrationsDir = path.join(repoRoot, "migrations");
const migrationFiles = ["0001_init.sql", "0002_unique_keys.sql"].map((name) => {
  const filePath = path.join(migrationsDir, name);
  return { name, path: filePath, sql: readFileSync(filePath, "utf8") };
});
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const D5_RUNTIME_STATES = ["cold", "starting", "ready", "degraded", "disabled"];
// 附录 C 显式 `CREATE INDEX` 数量（idx_events_session_seq 已由 0002 删除并收敛为 UNIQUE 索引）。
const EXPECTED_EXPLICIT_INDEXES = 5;

// ===== SQL 解析工具 =====

function stripComments(sql) {
  return sql
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, ""))
    .join("\n");
}

function splitStatements(sql) {
  const statements = [];
  let current = "";
  let depth = 0;
  let inString = false;
  for (let index = 0; index < sql.length; index += 1) {
    const char = sql[index];
    if (inString) {
      current += char;
      if (char === "'") {
        if (sql[index + 1] === "'") {
          current += sql[index + 1];
          index += 1;
        } else {
          inString = false;
        }
      }
      continue;
    }
    if (char === "'") {
      inString = true;
      current += char;
      continue;
    }
    if (char === "(") depth += 1;
    if (char === ")") depth -= 1;
    if (char === ";" && depth === 0) {
      if (current.trim()) statements.push(current.trim());
      current = "";
      continue;
    }
    current += char;
  }
  if (current.trim()) statements.push(current.trim());
  return statements;
}

function ddlStatements(sql) {
  return splitStatements(stripComments(sql))
    .map((statement) => statement.replace(/\s+/g, " ").trim())
    .filter((statement) => !/^PRAGMA\b/i.test(statement));
}

function appendixC() {
  const doc = readFileSync(path.join(repoRoot, "设计文档.md"), "utf8");
  const start = doc.indexOf("## 附录 C");
  const codeStart = doc.indexOf("```sql", start);
  const codeEnd = doc.indexOf("```", codeStart + 6);
  if (start < 0 || codeStart < 0 || codeEnd < 0) {
    throw new Error("设计文档缺少附录 C 的 sql 代码块");
  }
  return doc.slice(codeStart + 6, codeEnd);
}

function splitTopLevel(body) {
  const parts = [];
  let depth = 0;
  let current = "";
  for (const char of body) {
    if (char === "(") depth += 1;
    if (char === ")") depth -= 1;
    if (char === "," && depth === 0) {
      parts.push(current);
      current = "";
      continue;
    }
    current += char;
  }
  parts.push(current);
  return parts;
}

function splitColumns(list) {
  return list
    .split(",")
    .map((column) => column.trim())
    .filter(Boolean);
}

function parseColumn(column, type, rest) {
  const defaultValue = /\bDEFAULT\s+('(?:[^']|'')*'|[^\s]+)/.exec(rest);
  return {
    name: column,
    type: type.toUpperCase(),
    notnull: /\bNOT NULL\b/.test(rest) ? 1 : 0,
    dflt_value: defaultValue ? defaultValue[1] : null,
    pk: /\bPRIMARY KEY\b/.test(rest) ? 1 : 0,
  };
}

function parseTable(name, body) {
  const columns = [];
  const foreignKeys = [];
  const uniques = [];
  for (const rawPart of splitTopLevel(body)) {
    const part = rawPart.trim();
    if (!part) continue;
    if (/^(CHECK|UNIQUE|PRIMARY|FOREIGN|CONSTRAINT)\b/i.test(part)) {
      const unique = /^UNIQUE\s*\(([^)]*)\)$/i.exec(part);
      if (unique) uniques.push(splitColumns(unique[1]));
      continue;
    }
    const match = /^(\w+)\s+([A-Za-z]+)([\s\S]*)$/.exec(part);
    if (!match) throw new Error(`表 ${name}：无法解析列定义「${part}」`);
    const [, column, type, rest] = match;
    const reference = /\bREFERENCES\s+(\w+)\s*\(\s*(\w+)\s*\)([\s\S]*)$/.exec(rest);
    if (reference) {
      const onDelete = /\bON DELETE (SET NULL|CASCADE|SET DEFAULT|RESTRICT|NO ACTION)\b/.exec(
        reference[3],
      );
      foreignKeys.push({
        from: column,
        table: reference[1],
        to: reference[2],
        on_delete: onDelete ? onDelete[1] : "NO ACTION",
      });
    }
    columns.push(parseColumn(column, type, rest));
  }
  return { name, columns, foreignKeys, uniques };
}

/** 解析 SQL 片段为 { tables, indexes }（容忍 ALTER/DROP，供迁移集有效 schema 使用）。 */
function applyDdl(model, sql, source) {
  for (const statement of ddlStatements(sql)) {
    let match = /^CREATE TABLE\s+(\w+)\s*\(([\s\S]*)\)$/i.exec(statement);
    if (match) {
      model.tables.set(match[1], parseTable(match[1], match[2]));
      continue;
    }
    match = /^ALTER TABLE\s+(\w+)\s+ADD COLUMN\s+(\w+)\s+([A-Za-z]+)([\s\S]*)$/i.exec(statement);
    if (match) {
      const table = model.tables.get(match[1]);
      if (!table) throw new Error(`${source}: ALTER TABLE 目标不存在「${match[1]}」`);
      if (table.columns.some((column) => column.name === match[2])) {
        throw new Error(`${source}: 列已存在「${match[1]}.${match[2]}」`);
      }
      table.columns.push(parseColumn(match[2], match[3], match[4]));
      continue;
    }
    match = /^DROP INDEX\s+(\w+)$/i.exec(statement);
    if (match) {
      const index = model.indexes.findIndex((item) => item.name === match[1]);
      if (index < 0) throw new Error(`${source}: DROP INDEX 目标不存在「${match[1]}」`);
      model.indexes.splice(index, 1);
      continue;
    }
    match = /^CREATE UNIQUE INDEX\s+(\w+)\s+ON\s+(\w+)\s*\(([^)]*)\)$/i.exec(statement);
    if (match) {
      model.indexes.push({
        name: match[1],
        table: match[2],
        columns: splitColumns(match[3]),
        unique: true,
      });
      continue;
    }
    match = /^CREATE INDEX\s+(\w+)\s+ON\s+(\w+)\s*\(([^)]*)\)$/i.exec(statement);
    if (match) {
      model.indexes.push({
        name: match[1],
        table: match[2],
        columns: splitColumns(match[3]),
        unique: false,
      });
      continue;
    }
    throw new Error(`${source}: 出现未识别 DDL 语句：${statement}`);
  }
}

/** 迁移集的有效 schema（按版本顺序应用；与真实库执行结果同构）。 */
function effectiveMigrationModel() {
  const model = { tables: new Map(), indexes: [] };
  for (const file of migrationFiles) applyDdl(model, file.sql, file.name);
  return model;
}

/** 附录 C 的期望模型。 */
function appendixModel() {
  const model = { tables: new Map(), indexes: [] };
  applyDdl(model, appendixC(), "附录 C");
  return model;
}

// ===== 断言工具 =====

function assertDeepEqual(actual, expected, label) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) {
    throw new Error(`${label} 不一致\n  期望: ${e}\n  实际: ${a}`);
  }
}

function normalizeForeignKeys(keys) {
  return keys
    .map((key) => `${key.from} → ${key.table}(${key.to}) [ON DELETE ${key.on_delete}]`)
    .sort();
}

function uniqueKey(table, columns) {
  return `${table}(${columns.join(",")})`;
}

function declaredUniqueKeys(model) {
  const declared = new Set();
  const tables = model.tables instanceof Map ? [...model.tables.values()] : model.tables;
  for (const table of tables) {
    for (const unique of table.uniques ?? []) declared.add(uniqueKey(table.name, unique));
  }
  return declared;
}

function compareColumnSets(actualColumns, expectedColumns, label) {
  const actual = new Map(actualColumns.map((column) => [column.name, column]));
  const expected = new Map(expectedColumns.map((column) => [column.name, column]));
  assertDeepEqual([...actual.keys()].sort(), [...expected.keys()].sort(), `${label} 列清单`);
  for (const [name, column] of expected) {
    assertDeepEqual(actual.get(name), column, `${label}.${name} 定义（类型/NOT NULL/默认值/主键）`);
  }
}

/**
 * 校验资产（迁移有效模型或真实库 dump）与附录 C 一致。
 *
 * 规则：
 *  - 列按名称集合比较（迁移增量以 ALTER 追加列，SQLite 不保证与附录书写顺序一致）；
 *  - 表级 UNIQUE 可由唯一索引（任意 origin，unique=1）承载；
 *  - 显式非唯一索引必须与附录 C 的 `CREATE INDEX` 集合一致；
 *  - 额外索引只允许是匹配声明 UNIQUE 的唯一索引。
 */
function compareModels(actual, expected, label, options = {}) {
  const actualTables = new Map(
    (actual.tables instanceof Map ? [...actual.tables.values()] : actual.tables).map((table) => [
      table.name,
      table,
    ]),
  );
  const actualIndexes = actual.indexes.map((index) => ({
    name: index.name,
    table: index.table ?? index.tbl_name,
    columns: index.columns,
    unique: index.unique === 1 || index.unique === true,
    origin: index.origin,
  }));

  assertDeepEqual(
    [...actualTables.keys()].sort(),
    [...expected.tables.keys()].sort(),
    `${label} 表清单`,
  );

  const declaredUniques = declaredUniqueKeys(expected);
  const actualUniqueKeys = new Set(declaredUniqueKeys(actual));
  for (const index of actualIndexes) {
    if (index.unique) actualUniqueKeys.add(uniqueKey(index.table, index.columns));
  }
  const expectedExplicit = new Set(
    expected.indexes
      .filter((index) => !index.unique)
      .map((index) => `${index.name}(${index.columns.join(",")})`),
  );

  let columnCount = 0;
  let foreignKeyCount = 0;
  let uniqueCount = 0;

  for (const [name, table] of expected.tables) {
    const actualTable = actualTables.get(name);
    compareColumnSets(actualTable.columns, table.columns, `${label}.${name}`);
    columnCount += table.columns.length;

    assertDeepEqual(
      normalizeForeignKeys(actualTable.foreign_keys ?? actualTable.foreignKeys),
      normalizeForeignKeys(table.foreignKeys),
      `${label}.${name} 外键`,
    );
    foreignKeyCount += table.foreignKeys.length;

    for (const unique of table.uniques) {
      const key = uniqueKey(name, unique);
      if (!actualUniqueKeys.has(key)) {
        throw new Error(`${label}: ${name} 缺少 UNIQUE(${unique.join(", ")})`);
      }
      uniqueCount += 1;
    }
  }

  for (const index of actualIndexes) {
    // origin：'c' = CREATE INDEX；'u' = 表级 UNIQUE 自动索引；'pk' = 主键自动索引。
    if (index.origin === "u" || index.origin === "pk") continue;
    const key = `${index.name}(${index.columns.join(",")})`;
    if (expectedExplicit.has(key)) {
      if (index.unique) throw new Error(`${label}: 索引 ${key} 不应具有 UNIQUE`);
      continue;
    }
    if (!index.unique || !declaredUniques.has(uniqueKey(index.table, index.columns))) {
      throw new Error(`${label}: 出现附录 C 之外的索引 ${key}`);
    }
  }
  for (const key of expectedExplicit) {
    const name = key.slice(0, key.indexOf("("));
    if (!actualIndexes.some((index) => index.name === name)) {
      throw new Error(`${label}: 缺少显式索引 ${key}`);
    }
  }
  if (options.assertExplicitCount) {
    const explicit = actualIndexes.filter(
      (index) => !index.unique && (index.origin === undefined || index.origin === "c"),
    );
    if (explicit.length !== EXPECTED_EXPLICIT_INDEXES) {
      throw new Error(
        `${label}: 显式索引总数应为 ${EXPECTED_EXPLICIT_INDEXES}，实际 ${explicit.length}`,
      );
    }
  }

  console.log(
    `[live] 失败 schema 逐项一致：${expected.tables.size} 表 / ${columnCount} 列 / ` +
      `${foreignKeyCount} 外键 / ${uniqueCount} UNIQUE 约束 / ` +
      `${actualIndexes.filter((index) => !index.unique).length} 显式索引`,
  );
}

// ===== DoD2 静态：迁移文件卫生 =====

function checkMigrationHygiene() {
  for (const file of migrationFiles) {
    const stripped = stripComments(file.sql);
    if (/^\s*PRAGMA\b/im.test(stripped)) {
      throw new Error(`${file.name}: 迁移文件不得包含 PRAGMA（D3：连接打开时执行）`);
    }
    if (/\b(BEGIN|COMMIT|ROLLBACK|SAVEPOINT|DEFERRED)\b/i.test(stripped)) {
      throw new Error(`${file.name}: 迁移文件不得包含事务控制（单事务由迁移框架包裹）`);
    }
    if (/INSERT\s+INTO\s+schema_migrations/i.test(stripped)) {
      throw new Error(`${file.name}: 迁移文件不得写 schema_migrations`);
    }
    const sha256 = createHash("sha256").update(readFileSync(file.path)).digest("hex");
    console.log(`[static] ${file.name} 卫生通过；sha256=${sha256}`);
  }
}

// ===== DoD3 静态：迁移集有效 schema ↔ 附录 C =====

function checkMigrationsMatchAppendix() {
  compareModels(effectiveMigrationModel(), appendixModel(), "static", {
    assertExplicitCount: true,
  });
}

// ===== DoD5 静态：ADR-004 决策 6 / ADR-005 决策 2 =====

function checkIncrementalMigrationPolicy() {
  const [init, unique] = migrationFiles;
  if (/client_msg_id|UNIQUE\s*\(\s*session_id/i.test(init.sql)) {
    throw new Error("0001_init.sql 必须保持已发布内容（不得含 ADR-004/005 差异）");
  }
  const required = [
    /ALTER\s+TABLE\s+messages\s+ADD\s+COLUMN\s+client_msg_id\s+TEXT\s*;/i,
    /DROP\s+INDEX\s+idx_events_session_seq\s*;/i,
    /CREATE\s+UNIQUE\s+INDEX\s+idx_events_session_seq_uq\s+ON\s+events\s*\(\s*session_id\s*,\s*seq\s*\)\s*;/i,
    /CREATE\s+UNIQUE\s+INDEX\s+idx_messages_client_msg\s+ON\s+messages\s*\(\s*session_id\s*,\s*client_msg_id\s*\)\s*;/i,
  ];
  for (const pattern of required) {
    if (!pattern.test(unique.sql)) {
      throw new Error(`0002 迁移缺少必要语句：${pattern}`);
    }
  }
  const embedded = readFileSync(
    path.join(repoRoot, "crates", "aether-store", "src", "migration.rs"),
    "utf8",
  );
  if (!/0001_init\.sql/.test(embedded) || !/0002_unique_keys\.sql/.test(embedded)) {
    throw new Error("EMBEDDED_MIGRATIONS 必须同时内嵌 0001 与 0002");
  }
  console.log("[static] 0001 未改；0002 增量迁移（client_msg_id + 双 UNIQUE + 冗余索引删除）在案");
}

// ===== DoD5 实时：重复键审计（audit-duplicate-keys.mjs） =====

function rustExample(example, args) {
  const result = spawnSync(
    cargo,
    ["run", "--quiet", "-p", "aether-store", "--example", example, "--", ...args],
    { cwd: repoRoot, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, shell: false },
  );
  if (result.error) throw new Error(`无法运行示例 ${example}: ${result.error.message}`);
  return result;
}

function runAudit(dbPath, outPath) {
  const result = spawnSync(
    process.execPath,
    [
      path.join(repoRoot, "scripts", "test", "m1-03", "audit-duplicate-keys.mjs"),
      dbPath,
      "--out",
      outPath,
    ],
    { cwd: repoRoot, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, shell: false },
  );
  if (result.error) throw new Error(`无法运行审计脚本: ${result.error.message}`);
  console.log(result.stdout ?? "");
  if (result.stderr) console.error(result.stderr);
  const report = existsSync(outPath) ? JSON.parse(readFileSync(outPath, "utf8")) : null;
  return { exit: result.status, report };
}

function checkDuplicateKeyAudit() {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const workDir = path.join(repoRoot, "scripts", "test", ".tmp", "m1-03", stamp);
  mkdirSync(workDir, { recursive: true });

  const dirtyDb = path.join(workDir, "legacy-duplicates.db");
  const cleanDb = path.join(workDir, "upgraded.db");
  const dirtyReport = path.join(workDir, "audit-duplicates.json");
  const cleanReport = path.join(workDir, "audit-clean.json");

  // 1) 构造 pre-0002 演练库（events 重复 seq×2 组、messages 重复 client_msg_id×1 组）。
  const fixture = rustExample("audit_duplicates", [
    "create-fixture",
    dirtyDb,
    "--client-msg-id",
  ]);
  if (fixture.status !== 0) throw new Error(`构造演练库失败：${fixture.stderr ?? ""}`);

  const dirty = runAudit(dirtyDb, dirtyReport);
  if (dirty.exit !== 2 || !dirty.report) {
    throw new Error(`含重复键的库审计应 exit=2，实际 exit=${dirty.exit}`);
  }
  if (
    dirty.report.events.duplicate_groups !== 2 ||
    dirty.report.events.duplicate_rows !== 3
  ) {
    throw new Error(`events 重复统计不符：${JSON.stringify(dirty.report.events)}`);
  }
  if (
    dirty.report.messages.duplicate_groups !== 1 ||
    dirty.report.messages.duplicate_rows !== 1
  ) {
    throw new Error(`messages 重复统计不符：${JSON.stringify(dirty.report.messages)}`);
  }
  if (dirty.report.ok !== false) throw new Error("重复键库的审计结论必须为 ok=false");
  console.log(
    `[audit] 重复键样例识别通过；报告归档 ${path.relative(repoRoot, dirtyReport)}`,
  );

  // 2) 迁移到最新（0001 → 0002）后审计应无重复，且唯一约束已生效。
  const upgrade = rustExample("audit_duplicates", ["upgrade", cleanDb]);
  if (upgrade.status !== 0) throw new Error(`应用迁移失败：${upgrade.stderr ?? ""}`);
  const clean = runAudit(cleanDb, cleanReport);
  if (clean.exit !== 0 || !clean.report?.ok) {
    throw new Error(`干净库审计应 exit=0 且 ok=true，实际 exit=${clean.exit}`);
  }
  if (
    !clean.report.schema.events_unique_session_seq ||
    !clean.report.schema.messages_unique_client_msg
  ) {
    throw new Error("迁移后必须存在两个 UNIQUE 索引");
  }
  console.log(`[audit] 干净库审计通过；报告归档 ${path.relative(repoRoot, cleanReport)}`);

  // 3) 审计为只读：审计前后库文件字节一致。
  const before = createHash("sha256").update(readFileSync(cleanDb)).digest("hex");
  runAudit(cleanDb, path.join(workDir, "audit-clean-2.json"));
  const after = createHash("sha256").update(readFileSync(cleanDb)).digest("hex");
  if (before !== after) throw new Error("审计脚本不得修改数据库文件（只读）");

  writeFileSync(
    path.join(workDir, "summary.txt"),
    `dirty exit=${dirty.exit} events=${dirty.report.events.duplicate_groups} messages=${dirty.report.messages.duplicate_groups}\n` +
      `clean exit=${clean.exit} ok=${clean.report.ok}\n`,
    "utf8",
  );
  // 清理演练库（报告保留归档）。
  rmSync(dirtyDb, { force: true });
  rmSync(cleanDb, { force: true });
}

// ===== DoD3 实时：真实库结构 ↔ 附录 C =====

function runSchemaDump() {
  const result = spawnSync(
    cargo,
    ["run", "--quiet", "-p", "aether-store", "--example", "schema_dump"],
    { cwd: repoRoot, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, shell: false },
  );
  if (result.error) throw new Error(`无法运行 schema_dump: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`schema_dump 退出码 ${result.status}\n${result.stderr ?? ""}`);
  }
  const stdout = (result.stdout ?? "").trim();
  const firstBrace = stdout.indexOf("{");
  if (firstBrace < 0) throw new Error(`schema_dump 输出不是 JSON: ${stdout.slice(0, 200)}`);
  return JSON.parse(stdout.slice(firstBrace));
}

/** schema_dump 的按表嵌套结构 → compareModels 使用的统一模型。 */
function dumpToModel(dump) {
  const tables = new Map();
  const indexes = [];
  for (const table of dump.tables) {
    tables.set(table.name, {
      name: table.name,
      // 归一化字段顺序（JSON.stringify 对键序敏感；dump 的键序为字母序）。
      columns: table.columns.map((column) => ({
        name: column.name,
        type: column.type,
        notnull: column.notnull,
        dflt_value: column.dflt_value,
        pk: column.pk,
      })),
      foreignKeys: table.foreign_keys,
      uniques: [],
    });
    for (const index of table.indexes) {
      indexes.push({
        name: index.name,
        table: table.name,
        columns: index.columns,
        unique: index.unique,
        origin: index.origin,
      });
    }
  }
  return { tables, indexes };
}

// ===== 执行 =====

// schema_dump 常量（迁移集静态检查在无库环境下先跑）。
const staticChecks = [
  ["静态：迁移文件不含 PRAGMA/事务控制/版本写入", checkMigrationHygiene],
  ["静态：迁移集有效 schema ↔ 附录 C（逐项）", checkMigrationsMatchAppendix],
  ["静态：ADR-004/ADR-005 增量迁移策略（0001 未改 + 0002 语句）", checkIncrementalMigrationPolicy],
  [
    "静态：events.type 无 CHECK 枚举（D12）",
    () => {
      const events = effectiveMigrationModel().tables.get("events");
      if (!events) throw new Error("迁移缺少 events 表");
      const sql = ddlStatements(migrationFiles.map((file) => file.sql).join("\n")).find((item) =>
        /^CREATE TABLE events\b/i.test(item),
      );
      if (/\bCHECK\b/i.test(sql)) {
        throw new Error("events.type 必须为 TEXT + 应用层校验（D12），不得加 CHECK 枚举");
      }
      console.log("[static] events 表无 CHECK 枚举（D12）");
    },
  ],
  [
    "静态：runtimes.status CHECK == D5 状态机",
    () => {
      const statement = ddlStatements(migrationFiles.map((file) => file.sql).join("\n")).find(
        (item) => /^CREATE TABLE runtimes\b/i.test(item),
      );
      if (!statement) throw new Error("迁移缺少 runtimes 表");
      const check = /CHECK\s*\(\s*status\s+IN\s*\(([^)]*)\)/i.exec(statement);
      if (!check) throw new Error("runtimes.status 缺少 CHECK 枚举");
      const states = splitColumns(check[1]).map((value) => value.replace(/^'|'$/g, ""));
      assertDeepEqual(states, D5_RUNTIME_STATES, "runtimes.status CHECK（D5 状态机）");
      console.log(`[static] runtimes.status CHECK == D5 状态机: ${states.join(" → ")}`);
    },
  ],
];

for (const [name, check] of staticChecks) {
  try {
    check();
    record(name, 0);
  } catch (error) {
    console.error(`[static] 失败：${error.message}`);
    record(name, 1);
  }
}

try {
  compareModels(dumpToModel(runSchemaDump()), appendixModel(), "live", {
    assertExplicitCount: true,
  });
  record("实时：真实库结构 ↔ 附录 C（表/列/默认值/外键/索引/UNIQUE）", 0);
} catch (error) {
  console.error(`[live] 失败：${error.stack ?? error.message}`);
  record("实时：真实库结构 ↔ 附录 C（表/列/默认值/外键/索引/UNIQUE）", 1);
}

try {
  checkDuplicateKeyAudit();
  record("实时：重复键审计脚本（只读；含重复/干净双样例 + 报告归档）", 0);
} catch (error) {
  console.error(`[audit] 失败：${error.message}`);
  record("实时：重复键审计脚本（只读；含重复/干净双样例 + 报告归档）", 1);
}

record(
  "cargo test -p aether-store --test m1_03_pragma（DoD1 PRAGMA 断言）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_pragma"]),
);
record(
  "cargo test -p aether-store --test m1_03_migration（DoD2/DoD5 幂等/checksum/篡改拒绝/增量迁移）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_migration"]),
);
record(
  "cargo test -p aether-store --test m1_03_schema（DoD3 表/外键/索引/CHECK/UNIQUE 行为）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_schema"]),
);
record(
  "cargo test -p aether-store --test m1_03_safe_mode（DoD4 损坏库安全模式）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_safe_mode"]),
);

process.exit(summarize("verify-m1-03", checks));

