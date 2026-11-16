#!/usr/bin/env node
/**
 * M10-09 标准数据集 generation 构造（ADR-039 D-039-1）。
 *
 * 从 M10-08 单库数据集（`dist-out/m10-09/dataset/aether.db`）构造产品 generation：
 * `<out>/generations/<gen>/aether.db` + 8 个 `events/events-<n>.db` + `shards.json` + `CURRENT`。
 *
 * - 主库只保留 sessions/messages/runs/audit 等；`events` 恒空；
 * - 分片按 **会话稳定哈希 FNV-1a 64 mod 8**（与 Rust `layout::shard_for` 同算法）；
 * - 每片执行完整 0001–0014 schema（不跳过）；
 * - 只写 `--out`，不触碰用户库；不改变产品代码/迁移。
 *
 * 用法：
 *   node scripts/test/m10-09/build-generation.mjs \
 *     --dataset dist-out/m10-09/dataset --out dist-out/m10-09/generation --force
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import {
  applyMigrations,
  openDatabase,
  repoRootFrom,
  writeJson,
} from "../m10-08/lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const GENERATION = "gen-01J8ZQ00000000000000000001";

function parseArgs(argv) {
  const options = {
    dataset: path.join(repoRoot, "dist-out", "m10-09", "dataset"),
    out: path.join(repoRoot, "dist-out", "m10-09", "generation"),
    force: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--dataset":
        options.dataset = path.resolve(repoRoot, next());
        break;
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--force":
        options.force = true;
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  return options;
}

/** 与 Rust layout::shard_for 完全一致：UTF-8 字节 FNV-1a 64 mod 8。 */
function shardFor(sessionId) {
  let hash = 0xcbf29ce484222325n;
  for (const byte of Buffer.from(sessionId, "utf8")) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return Number(hash % 8n);
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function ensureDir(dir) {
  mkdirSync(dir, { recursive: true });
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const sourcePath = path.join(options.dataset, "aether.db");
  if (!existsSync(sourcePath)) {
    throw new Error(`数据集不存在：${sourcePath}（先运行 gen-events-dataset.mjs）`);
  }
  const out = options.out;
  if (options.force && existsSync(out)) {
    rmSync(out, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
  ensureDir(path.join(out, "generations", GENERATION, "events"));
  const generationDir = path.join(out, "generations", GENERATION);
  const mainPath = path.join(generationDir, "aether.db");

  console.log(`[m10-09-build] source=${sourcePath}`);
  console.log(`[m10-09-build] generation=${GENERATION}`);

  // ---- 主库：schema + 复制非 events 表（FTS rebuild） ----
  const mainDb = openDatabase(mainPath);
  mainDb.exec("PRAGMA journal_mode = WAL");
  mainDb.exec("PRAGMA foreign_keys = OFF");
  applyMigrations(mainDb, repoRoot);
  mainDb.prepare("ATTACH DATABASE ? AS src").run(sourcePath);

  const tables = mainDb
    .prepare(
      "SELECT name FROM src.sqlite_master WHERE type='table' " +
        "AND name NOT LIKE 'sqlite_%' AND name != 'events' " +
        "AND name != 'schema_migrations' AND name NOT LIKE '%_fts_%' " +
        "AND name NOT IN ('sessions_fts','messages_fts') ORDER BY name",
    )
    .all()
    .map((row) => row.name);
  for (const table of tables) {
    mainDb.exec(`INSERT OR IGNORE INTO main."${table}" SELECT * FROM src."${table}"`);
  }
  mainDb.exec("DELETE FROM main.events");
  // 主库 sessions.shard_id 与稳定哈希一致。
  const updateShard = mainDb.prepare(
    "UPDATE main.sessions SET shard_id = ?2, deletion_pending = 0 WHERE id = ?1",
  );
  const sessions = mainDb.prepare("SELECT id FROM main.sessions ORDER BY id").all();
  for (const row of sessions) updateShard.run(row.id, shardFor(row.id));
  // FTS 由 main 表重建（源 FTS 影子表未复制）。
  try {
    mainDb.exec("INSERT INTO main.sessions_fts(sessions_fts) VALUES('rebuild')");
    mainDb.exec("INSERT INTO main.messages_fts(messages_fts) VALUES('rebuild')");
  } catch (error) {
    console.warn(`[m10-09-build] FTS rebuild 跳过：${error.message}`);
  }
  mainDb.prepare("DETACH DATABASE src").run();
  mainDb.exec("PRAGMA foreign_keys = ON");
  mainDb.exec("PRAGMA wal_checkpoint(TRUNCATE)");
  mainDb.close();

  // ---- 8 分片：完整 0014 schema + 单次顺序扫描源库、按会话稳定哈希路由 ----
  // 说明：源库在 SMR HDD 上，逐分片 `INSERT ... SELECT` 会重复随机索引读，
  // 实测 ~14min/片；这里改为一次 `ORDER BY rowid` 顺序扫描 + JS 路由到 8 个
  // 目标库（目标目录建议 SSD），源库只读一次。
  const shardSessions = Array.from({ length: 8 }, () => []);
  for (const row of sessions) shardSessions[shardFor(row.id)].push(row.id);

  const shardEntries = [];
  const shardDbs = [];
  for (let shard = 0; shard < 8; shard += 1) {
    const relative = `events/events-${shard}.db`;
    const shardPath = path.join(generationDir, relative);
    const shardDb = openDatabase(shardPath);
    shardDb.exec("PRAGMA auto_vacuum = INCREMENTAL");
    shardDb.exec("PRAGMA journal_mode = OFF");
    shardDb.exec("PRAGMA synchronous = OFF");
    applyMigrations(shardDb, repoRoot);
    shardDb.exec("DROP INDEX IF EXISTS idx_events_session_seq_uq");
    shardDb.exec("DROP INDEX IF EXISTS idx_events_type_ts");
    shardDb.exec("BEGIN");
    const insert = shardDb.prepare(
      "INSERT INTO main.events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) " +
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    );
    shardDbs.push({ shard, relative, db: shardDb, insert });
    shardEntries.push({ id: shard, path: relative, schema_version: 14 });
  }

  const sourceDb = openDatabase(sourcePath, { readOnly: true });
  const sessionShard = new Map(sessions.map((row) => [row.id, shardFor(row.id)]));
  const sourceRows = sourceDb
    .prepare(
      "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v " +
        "FROM events ORDER BY rowid",
    )
    .iterate();
  let processed = 0;
  for (const row of sourceRows) {
    const shard = sessionShard.get(row.session_id);
    if (shard === undefined) {
      throw new Error(`events.session_id 不在 sessions：${row.session_id}`);
    }
    shardDbs[shard].insert.run(
      row.id,
      row.session_id,
      row.run_id,
      row.runtime_id,
      row.seq,
      row.type,
      row.payload,
      row.ts,
      row.v,
    );
    processed += 1;
    if (processed % 50_000 === 0) {
      for (const entry of shardDbs) {
        entry.db.exec("COMMIT");
        entry.db.exec("BEGIN");
      }
      if (processed % 1_000_000 === 0) {
        console.log(`[m10-09-build] routed ${processed.toLocaleString()} events`);
      }
    }
  }
  sourceDb.close();
  for (const entry of shardDbs) {
    entry.db.exec("COMMIT");
    entry.db.exec(
      "CREATE UNIQUE INDEX idx_events_session_seq_uq ON events(session_id, seq)",
    );
    entry.db.exec("CREATE INDEX idx_events_type_ts ON events(type, ts)");
    entry.db.exec("PRAGMA journal_mode = WAL");
    entry.db.exec("PRAGMA synchronous = NORMAL");
    entry.db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
    entry.db.close();
    console.log(
      `[m10-09-build] shard-${entry.shard}: sessions=${shardSessions[entry.shard].length}`,
    );
  }
  console.log(`[m10-09-build] routed ${processed.toLocaleString()} events total`);

  const manifest = {
    shard_count: 8,
    shards: shardEntries,
    schema_version: 14,
    app_version: "0.1.0",
    created_at: Date.now(),
  };
  const manifestBytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  const manifestPath = path.join(generationDir, "events", "shards.json");
  writeFileSync(manifestPath, manifestBytes);
  const pointer = {
    format_version: 1,
    generation: GENERATION,
    shards_sha256: sha256(manifestBytes),
    switched_at: Date.now(),
  };
  writeFileSync(path.join(out, "CURRENT"), `${JSON.stringify(pointer, null, 2)}\n`);
  writeFileSync(
    path.join(out, "dataset.json"),
    readFileSync(path.join(options.dataset, "dataset.json")),
  );
  writeJson(path.join(out, "build.json"), {
    generation: GENERATION,
    sessions: sessions.length,
    shards: 8,
    manifest_sha256: pointer.shards_sha256,
  });
  console.log(`[m10-09-build] 完成：${out}`);
}

main();
