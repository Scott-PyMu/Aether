#!/usr/bin/env node
/**
 * M10-08 分库对照数据集构造（ADR-038 D-038-1 第 6 项）。
 *
 * 从单库数据集按会话分片到 N 个同 schema 的 DB；只读源库、写临时目录，
 * **不改变产品 schema / 命令面 / 迁移**，仅用于同数据集对照压测。
 *
 * 构造方式：单次顺序扫描源 `events ORDER BY rowid`，按会话路由到 N 个
 * shard（避免每个 shard 各自回源随机读，5GB 下快一个数量级）。
 *
 * 用法：
 *   node scripts/test/m10-08/build-shards.mjs --shards 8
 *   node scripts/test/m10-08/build-shards.mjs --dataset dist-out/m10-08/dataset \
 *     --out dist-out/m10-08/shards --shards 8 --force
 */
import { existsSync, readdirSync, rmSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import {
  applyMigrations,
  ensureDir,
  formatBytes,
  openDatabase,
  readJson,
  repoRootFrom,
  sessionIdFor,
  sessionIndexFromId,
  writeJson,
} from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);

function parseArgs(argv) {
  const options = {
    dataset: path.join(repoRoot, "dist-out", "m10-08", "dataset"),
    out: path.join(repoRoot, "dist-out", "m10-08", "shards"),
    shards: 8,
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
      case "--shards":
        options.shards = Number(next());
        break;
      case "--force":
        options.force = true;
        break;
      case "--help":
      case "-h":
        console.log("用法：node scripts/test/m10-08/build-shards.mjs [--dataset DIR] [--out DIR] [--shards N] [--force]");
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!Number.isInteger(options.shards) || options.shards < 2) throw new Error("--shards 必须 ≥2");
  return options;
}

function sqlString(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const sourcePath = path.join(options.dataset, "aether.db");
  const manifestPath = path.join(options.dataset, "dataset.json");
  if (!existsSync(sourcePath) || !existsSync(manifestPath)) {
    throw new Error(`数据集不存在：${options.dataset}（先运行 gen-events-dataset.mjs）`);
  }
  const dataset = readJson(manifestPath);
  if (options.force && existsSync(options.out)) {
    rmSync(options.out, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
  ensureDir(options.out);
  if (readdirSync(options.out).length > 0 && existsSync(path.join(options.out, "shards.json"))) {
    console.log(`[m10-08-shards] 已存在：${options.out}（如需重建加 --force）`);
    return;
  }

  const startedAt = Date.now();
  const shardSessionIds = Array.from({ length: options.shards }, (_, shard) => {
    const ids = [];
    for (let index = shard; index < dataset.sessions; index += options.shards) ids.push(sessionIdFor(index));
    return ids;
  });
  const sessionShard = new Map();
  for (let index = 0; index < dataset.sessions; index += 1) {
    sessionShard.set(sessionIdFor(index), index % options.shards);
  }

  const dbs = [];
  const insertEvents = [];
  const files = [];
  for (let shard = 0; shard < options.shards; shard += 1) {
    const name = `shard-${shard}.db`;
    const dbPath = path.join(options.out, name);
    if (existsSync(dbPath)) rmSync(dbPath, { force: true });
    const db = openDatabase(dbPath);
    db.exec("PRAGMA journal_mode = OFF");
    db.exec("PRAGMA synchronous = OFF");
    db.exec("PRAGMA cache_size = -65536");
    applyMigrations(db, repoRoot);
    db.exec("DROP INDEX IF EXISTS idx_events_session_seq_uq");
    db.exec("DROP INDEX IF EXISTS idx_events_type_ts");

    // 小表 + 该分片会话的消息（走源库索引；消息总量仅 2 万，成本可忽略）。
    db.exec(`ATTACH DATABASE ${sqlString(sourcePath)} AS source`);
    const placeholders = shardSessionIds[shard].map(() => "?").join(", ");
    db.exec("INSERT INTO main.runtimes SELECT * FROM source.runtimes");
    db.exec("INSERT INTO main.workspaces SELECT * FROM source.workspaces");
    db.prepare(`INSERT INTO main.sessions SELECT * FROM source.sessions WHERE id IN (${placeholders})`).run(
      ...shardSessionIds[shard],
    );
    db.prepare(`INSERT INTO main.messages SELECT * FROM source.messages WHERE session_id IN (${placeholders})`).run(
      ...shardSessionIds[shard],
    );
    db.exec("DETACH DATABASE source");

    insertEvents.push(
      db.prepare(
        "INSERT INTO events(id, session_id, run_id, runtime_id, seq, type, payload, ts, v) " +
          "VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1)",
      ),
    );
    dbs.push(db);
  }

  const source = openDatabase(sourcePath, { readOnly: true });
  const sourceRows = Number(source.prepare("SELECT COUNT(*) AS count FROM events").get().count);
  const select = source.prepare(
    "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v FROM events ORDER BY rowid",
  );
  for (const db of dbs) db.exec("BEGIN");

  const shardRows = new Array(options.shards).fill(0);
  let rows = 0;
  let lastReport = 0;
  for (const event of select.iterate()) {
    const shard = sessionShard.get(String(event.session_id));
    if (shard === undefined) throw new Error(`未知 session_id：${event.session_id}`);
    insertEvents[shard].run(
      event.id,
      event.session_id,
      event.run_id,
      event.runtime_id,
      event.seq,
      event.type,
      event.payload,
      event.ts,
    );
    shardRows[shard] += 1;
    rows += 1;
    if (rows % 100_000 === 0) {
      for (const db of dbs) {
        db.exec("COMMIT");
        db.exec("BEGIN");
      }
    }
    if (rows - lastReport >= 2_000_000) {
      lastReport = rows;
      console.log(`[m10-08-shards] 路由 ${rows.toLocaleString()} / ${sourceRows.toLocaleString()} rows…`);
    }
  }
  for (const db of dbs) db.exec("COMMIT");
  source.close();

  for (let shard = 0; shard < options.shards; shard += 1) {
    const db = dbs[shard];
    const indexStarted = Date.now();
    db.exec("CREATE UNIQUE INDEX idx_events_session_seq_uq ON events(session_id, seq)");
    db.exec("CREATE INDEX idx_events_type_ts ON events(type, ts)");
    db.exec("ANALYZE");
    db.exec("PRAGMA journal_mode = WAL");
    db.exec("PRAGMA synchronous = NORMAL");
    db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
    db.close();
    const dbPath = path.join(options.out, `shard-${shard}.db`);
    const bytes = statSync(dbPath).size;
    files.push({
      shard,
      db: path.basename(dbPath),
      sessions: shardSessionIds[shard].length,
      events: shardRows[shard],
      bytes,
      buildMs: Date.now() - indexStarted,
    });
    console.log(
      `[m10-08-shards] shard-${shard}: ${shardSessionIds[shard].length} sessions / ` +
        `${shardRows[shard].toLocaleString()} events / ${formatBytes(bytes)}`,
    );
  }

  const totalEvents = files.reduce((sum, entry) => sum + entry.events, 0);
  const totalBytes = files.reduce((sum, entry) => sum + entry.bytes, 0);
  if (totalEvents !== sourceRows) {
    throw new Error(`分片事件数不一致：source=${sourceRows} / shards=${totalEvents}`);
  }
  const manifest = {
    task: "M10-08",
    method: "按会话分片（session index % shards）；单次顺序扫描源库路由",
    source: path.relative(repoRoot, sourcePath).replaceAll("\\", "/"),
    sourceRows,
    sourceBytes: dataset.bytes,
    shards: options.shards,
    files,
    totalEvents,
    totalBytes,
    buildMs: Date.now() - startedAt,
    generatedAt: new Date().toISOString(),
  };
  writeJson(path.join(options.out, "shards.json"), manifest);
  console.log(
    `[m10-08-shards] OK：${options.shards} shards / ${totalEvents.toLocaleString()} events / ` +
      `${formatBytes(totalBytes)} / ${((Date.now() - startedAt) / 1000).toFixed(1)}s`,
  );
}

try {
  main();
} catch (error) {
  console.error(`[m10-08-shards] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
}
