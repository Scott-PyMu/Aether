/**
 * M1-03 验证脚本：SQLite 存储层与迁移（D3、D12、评审#2、附录 C）。
 *
 * 覆盖 DoD：
 *   1) PRAGMA 断言：由 Rust 集成测试在真实库上逐项断言（journal_mode=wal / synchronous=normal /
 *      foreign_keys=on / busy_timeout=5000 / wal_autocheckpoint=1000 / journal_size_limit=67108864 /
 *      cache_size=-32000，另按 D3 含 temp_store=MEMORY）；
 *   2) 迁移幂等 + schema_migrations(version+checksum) + 篡改拒绝：Rust 集成测试；
 *   3) 附录 C 全部表/列/默认值/外键/索引逐项断言：
 *      - 静态：迁移 0001 的 DDL 与设计文档附录 C 逐字一致（忽略注释与 PRAGMA）；
 *      - 实时：schema_dump 示例从真实库导出结构，与附录 C 解析结果逐项比对；
 *   4) 损坏库 → 安全模式（只读 + 备份/导出入口 + 拒绝写入）：Rust 集成测试。
 *
 * 说明：迁移文件内容经 `include_bytes!` 内嵌，sha256 即校验口径（D3 评审修订 #2）；
 * 本脚本额外输出磁盘文件 sha256 作为证据。
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const migrationPath = path.join(repoRoot, "migrations", "0001_init.sql");
const migrationSql = readFileSync(migrationPath, "utf8");
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const D5_RUNTIME_STATES = ["cold", "starting", "ready", "degraded", "disabled"];
const EXPECTED_EXPLICIT_INDEXES = 6;

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
    const defaultValue = /\bDEFAULT\s+('(?:[^']|'')*'|[^\s]+)/.exec(rest);
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
    columns.push({
      name: column,
      type: type.toUpperCase(),
      notnull: /\bNOT NULL\b/.test(rest) ? 1 : 0,
      dflt_value: defaultValue ? defaultValue[1] : null,
      pk: /\bPRIMARY KEY\b/.test(rest) ? 1 : 0,
    });
  }
  return { name, columns, foreignKeys, uniques };
}

function parseDdl(sql) {
  const tables = new Map();
  const indexes = [];
  for (const statement of ddlStatements(sql)) {
    let match = /^CREATE TABLE\s+(\w+)\s*\(([\s\S]*)\)$/i.exec(statement);
    if (match) {
      tables.set(match[1], parseTable(match[1], match[2]));
      continue;
    }
    match = /^CREATE INDEX\s+(\w+)\s+ON\s+(\w+)\s*\(([^)]*)\)$/i.exec(statement);
    if (match) {
      indexes.push({
        name: match[1],
        table: match[2],
        columns: splitColumns(match[3]),
      });
      continue;
    }
    throw new Error(`出现未识别 DDL 语句：${statement}`);
  }
  return { tables, indexes };
}

// ===== 断言工具 =====

function assertDeepEqual(actual, expected, label) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) {
    throw new Error(`${label} 不一致\n  期望: ${e}\n  实际: ${a}`);
  }
}

// ===== DoD2 静态：迁移文件卫生 =====

function checkMigrationHygiene() {
  const stripped = stripComments(migrationSql);
  if (/^\s*PRAGMA\b/im.test(stripped)) {
    throw new Error("迁移文件不得包含 PRAGMA（D3：连接打开时执行，不在迁移文件中）");
  }
  if (/\b(BEGIN|COMMIT|ROLLBACK|SAVEPOINT)\b/i.test(stripped)) {
    throw new Error("迁移文件不得包含事务控制（单事务由迁移框架包裹）");
  }
  if (/INSERT\s+INTO\s+schema_migrations/i.test(stripped)) {
    throw new Error("迁移文件不得写 schema_migrations（版本记录由迁移框架负责）");
  }
  const sha256 = createHash("sha256").update(readFileSync(migrationPath)).digest("hex");
  console.log(`[static] 迁移文件卫生通过；0001_init.sql sha256=${sha256}`);
}

// ===== DoD3 静态：迁移 0001 与附录 C 逐字一致 =====

function checkAppendixCEqualsMigration() {
  const appendix = ddlStatements(appendixC());
  const migration = ddlStatements(migrationSql);
  if (appendix.length !== migration.length) {
    throw new Error(`DDL 语句数不一致：附录 C=${appendix.length} 迁移=${migration.length}`);
  }
  for (let index = 0; index < appendix.length; index += 1) {
    if (appendix[index] !== migration[index]) {
      throw new Error(
        `第 ${index + 1} 条 DDL 不一致\n  附录 C: ${appendix[index]}\n  迁移  : ${migration[index]}`,
      );
    }
  }
  console.log(`[static] 迁移 0001 与附录 C 逐字一致（${appendix.length} 条 DDL 语句）`);
}

// ===== DoD3 静态：D12/D5 专项 =====

function checkEventsTypeNoCheck() {
  const statement = ddlStatements(migrationSql).find((item) =>
    /^CREATE TABLE events\b/i.test(item),
  );
  if (!statement) throw new Error("迁移缺少 events 表");
  if (/\bCHECK\b/i.test(statement)) {
    throw new Error("events.type 必须为 TEXT + 应用层校验（D12），不得加 CHECK 枚举");
  }
  console.log("[static] events 表无 CHECK 枚举（D12）");
}

function checkRuntimeStatusStates() {
  const statement = ddlStatements(migrationSql).find((item) =>
    /^CREATE TABLE runtimes\b/i.test(item),
  );
  if (!statement) throw new Error("迁移缺少 runtimes 表");
  const check = /CHECK\s*\(\s*status\s+IN\s*\(([^)]*)\)/i.exec(statement);
  if (!check) throw new Error("runtimes.status 缺少 CHECK 枚举");
  const states = splitColumns(check[1]).map((value) => value.replace(/^'|'$/g, ""));
  assertDeepEqual(states, D5_RUNTIME_STATES, "runtimes.status CHECK（D5 状态机）");
  console.log(`[static] runtimes.status CHECK == D5 状态机: ${states.join(" → ")}`);
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

function compareStructure(dump, expected) {
  const dumpTables = new Map(dump.tables.map((table) => [table.name, table]));
  assertDeepEqual(
    [...dumpTables.keys()].sort(),
    [...expected.tables.keys()].sort(),
    "表清单",
  );

  const normalizeForeignKeys = (keys) =>
    keys
      .map((key) => `${key.from} → ${key.table}(${key.to}) [ON DELETE ${key.on_delete}]`)
      .sort();

  let columnCount = 0;
  let foreignKeyCount = 0;
  let indexCount = 0;
  let uniqueCount = 0;

  for (const [name, table] of expected.tables) {
    const actual = dumpTables.get(name);
    assertDeepEqual(
      actual.columns.map((column) => ({
        name: column.name,
        type: column.type,
        notnull: column.notnull,
        dflt_value: column.dflt_value,
        pk: column.pk,
      })),
      table.columns,
      `${name} 列（名称/类型/NOT NULL/默认值/主键）`,
    );
    columnCount += table.columns.length;

    assertDeepEqual(
      normalizeForeignKeys(actual.foreign_keys),
      normalizeForeignKeys(table.foreignKeys),
      `${name} 外键`,
    );
    foreignKeyCount += table.foreignKeys.length;

    const expectedIndexes = expected.indexes
      .filter((index) => index.table === name)
      .map((index) => `${index.name}(${index.columns.join(",")})`)
      .sort();
    const actualIndexes = actual.indexes
      .filter((index) => index.origin === "c")
      .map((index) => `${index.name}(${index.columns.join(",")})`)
      .sort();
    assertDeepEqual(actualIndexes, expectedIndexes, `${name} 显式索引`);
    indexCount += expectedIndexes.length;

    for (const unique of table.uniques) {
      const found = actual.indexes.some(
        (index) => index.origin === "u" && index.columns.join(",") === unique.join(","),
      );
      if (!found) throw new Error(`${name} 缺少 UNIQUE(${unique.join(", ")})`);
      uniqueCount += 1;
    }
  }

  if (indexCount !== EXPECTED_EXPLICIT_INDEXES) {
    throw new Error(`显式索引总数应为 ${EXPECTED_EXPLICIT_INDEXES}，实际 ${indexCount}`);
  }
  console.log(
    `[live] 真实库结构 ↔ 附录 C 逐项一致：${expected.tables.size} 表 / ${columnCount} 列 / ` +
      `${foreignKeyCount} 外键 / ${indexCount} 显式索引 / ${uniqueCount} UNIQUE 约束`,
  );
}

// ===== 执行 =====

const staticChecks = [
  ["静态：迁移文件不含 PRAGMA/事务控制/版本写入", checkMigrationHygiene],
  ["静态：迁移 0001 DDL 与附录 C 逐字一致", checkAppendixCEqualsMigration],
  ["静态：events.type 无 CHECK 枚举（D12）", checkEventsTypeNoCheck],
  ["静态：runtimes.status CHECK == D5 状态机", checkRuntimeStatusStates],
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
  compareStructure(runSchemaDump(), parseDdl(appendixC()));
  record("实时：真实库结构 ↔ 附录 C（表/列/默认值/外键/索引）", 0);
} catch (error) {
  console.error(`[live] 失败：${error.message}`);
  record("实时：真实库结构 ↔ 附录 C（表/列/默认值/外键/索引）", 1);
}

record(
  "cargo test -p aether-store --test m1_03_pragma（DoD1 PRAGMA 断言）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_pragma"]),
);
record(
  "cargo test -p aether-store --test m1_03_migration（DoD2 幂等/checksum/篡改拒绝）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_migration"]),
);
record(
  "cargo test -p aether-store --test m1_03_schema（DoD3 表/外键/索引/CHECK 行为）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_schema"]),
);
record(
  "cargo test -p aether-store --test m1_03_safe_mode（DoD4 损坏库安全模式）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_03_safe_mode"]),
);

process.exit(summarize("verify-m1-03", checks));
