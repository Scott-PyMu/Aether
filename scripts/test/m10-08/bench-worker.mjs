#!/usr/bin/env node
/**
 * M10-08 压测 worker（ADR-038 D-038-1）。
 *
 * worker_threads 模式：
 *   - `write`：每会话 1 writer，按固定速率产生事件，等待父线程 group commit ack；
 *   - `read`：10 并发补读 `events_page` / `messages_page` / `last_seq`。
 *
 * 子进程模式（避免父进程缓存/连接干扰，测冷启动与备份恢复）：
 *   - `quick-check`：打开 + D3 PRAGMA + `quick_check` + 迁移校验；
 *   - `backup`：`VACUUM INTO` + 备份 `quick_check`；
 *   - `restore-check`：复制后的库打开 + `quick_check` + 迁移校验；
 *   - `retention`：按 ADR-021 30 天口径删除可清理类型并统计 live bytes。
 */
import { parentPort, workerData } from "node:worker_threads";
import { existsSync, statSync } from "node:fs";

import {
  EVENTS_PAGE_SQL,
  LAST_SEQ_SQL,
  MESSAGES_PAGE_SQL,
  RETENTION_TYPES,
  applyMigrations,
  makePayload,
  mulberry32,
  openDatabase,
  pickEventType,
  repoRootFrom,
  sleep,
  summarize,
} from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);

function nowMs() {
  return Date.now();
}

function parseCli(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const value = argv[index + 1] && !argv[index + 1].startsWith("--") ? argv[++index] : "true";
    options[key] = value;
  }
  return options;
}

function round(value, digits = 3) {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

async function writeWorker(data) {
  const view = new Int32Array(data.shared);
  const random = mulberry32(data.seed + data.sessionIndex);
  const db = openDatabase(data.dbPath);
  let seq = Number(
    db.prepare("SELECT COALESCE(MAX(seq), 0) AS seq FROM events WHERE session_id = ?1").get(data.sessionId).seq,
  );
  const latencies = [];
  const wakeDelays = [];
  let events = 0;
  let ackValue = 0;
  let index = data.sessionIndex * 1_000_000;
  const interval = (data.batchSize * 1000) / data.ratePerWriter;
  const stagger = (interval * data.sessionIndex) / Math.max(1, data.writers);
  const firstSendAt = data.startAt + stagger;
  const initialSleep = firstSendAt - nowMs();
  if (initialSleep > 0) await sleep(initialSleep);
  while (nowMs() < data.deadline) {
    const sentAt = nowMs();
    const batch = [];
    for (let offset = 0; offset < data.batchSize; offset += 1) {
      const type = pickEventType(random);
      const payload = makePayload(type, index, random);
      seq += 1;
      index += 1;
      batch.push({
        id: `bench-${String(data.sessionIndex).padStart(2, "0")}-${String(seq).padStart(10, "0")}`,
        sessionId: data.sessionId,
        runId: `run-bench-${data.sessionIndex}`,
        runtimeId: "claude-code",
        seq,
        type,
        payload,
        ts: sentAt,
      });
    }
    ackValue += 1;
    parentPort.postMessage({ type: "batch", sessionId: data.sessionId, events: batch, sentAt, ackValue });
    const waitResult = Atomics.wait(view, 1, ackValue - 1, 120_000);
    if (waitResult === "timed-out") {
      throw new Error(`writer ${data.sessionId} 等待 group commit ack 超时`);
    }
    const latencyMs = nowMs() - sentAt;
    latencies.push(latencyMs);
    wakeDelays.push(Math.max(0, nowMs() - data.startedAt - Atomics.load(view, 2)));
    events += batch.length;
    const nextAt = sentAt + interval;
    if (nextAt > nowMs()) await sleep(nextAt - nowMs());
  }
  db.close();
  parentPort.postMessage({ type: "done", latencies, wakeDelays, events, sessionId: data.sessionId });
}

async function readWorker(data) {
  const databases = data.shards.map((shard) => {
    const db = openDatabase(shard.dbPath, { readOnly: true });
    return {
      db,
      sessions: shard.sessionIds,
      eventsPage: db.prepare(EVENTS_PAGE_SQL),
      messagesPage: db.prepare(MESSAGES_PAGE_SQL),
      lastSeq: db.prepare(LAST_SEQ_SQL),
    };
  });
  const samples = { eventsPage: [], messagesPage: [], lastSeq: [] };
  let queries = 0;
  const startSleep = data.startAt - nowMs();
  if (startSleep > 0) {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, startSleep);
  }
  while (nowMs() < data.deadline) {
    const target = databases[Math.floor(Math.random() * databases.length)];
    const session = target.sessions[Math.floor(Math.random() * target.sessions.length)];

    const t0 = performance.now();
    const max = Number(target.lastSeq.get(session).seq);
    samples.lastSeq.push(performance.now() - t0);

    const after = Math.random() < 0.9 ? Math.max(0, max - 500) : null;
    const t1 = performance.now();
    target.eventsPage.all(session, after, 50);
    samples.eventsPage.push(performance.now() - t1);

    const t2 = performance.now();
    target.messagesPage.all(session, null, 50);
    samples.messagesPage.push(performance.now() - t2);

    queries += 3;
    await sleep(20);
  }
  for (const entry of databases) entry.db.close();
  parentPort.postMessage({
    type: "done",
    queries,
    samples: {
      eventsPage: samples.eventsPage,
      messagesPage: samples.messagesPage,
      lastSeq: samples.lastSeq,
    },
  });
}

function quickCheckMode(options) {
  const dbPath = options.db;
  if (!existsSync(dbPath)) throw new Error(`库不存在：${dbPath}`);
  const totalStarted = performance.now();
  const openStarted = performance.now();
  const db = openDatabase(dbPath);
  const openMs = round(performance.now() - openStarted, 3);

  const checkStarted = performance.now();
  const report = db.prepare("PRAGMA quick_check").all();
  const quickCheckMs = round(performance.now() - checkStarted, 3);
  const ok = report.length === 1 && String(report[0].quick_check ?? report[0]["quick_check"]).toLowerCase() === "ok";

  const migrateStarted = performance.now();
  const applied = applyMigrations(db, repoRoot);
  const migrateMs = round(performance.now() - migrateStarted, 3);
  db.close();
  return {
    db: dbPath,
    ok,
    report: report.slice(0, 5),
    openMs,
    quickCheckMs,
    migrateMs,
    appliedMigrations: applied,
    totalMs: round(performance.now() - totalStarted, 3),
  };
}

function backupMode(options) {
  const dbPath = options.db;
  const out = options.out;
  if (!existsSync(dbPath)) throw new Error(`库不存在：${dbPath}`);
  if (existsSync(out)) throw new Error(`备份目标已存在：${out}`);
  const db = openDatabase(dbPath);
  const started = performance.now();
  const escaped = out.replaceAll("'", "''");
  db.exec(`VACUUM INTO '${escaped}'`);
  const vacuumMs = round(performance.now() - started, 3);
  const checkStarted = performance.now();
  const checkDb = openDatabase(out);
  const report = checkDb.prepare("PRAGMA quick_check").all();
  const checkMs = round(performance.now() - checkStarted, 3);
  const ok = report.length === 1 && String(report[0].quick_check).toLowerCase() === "ok";
  checkDb.close();
  db.close();
  return {
    source: dbPath,
    backup: out,
    backupBytes: statSync(out).size,
    ok,
    vacuumMs,
    verifyQuickCheckMs: checkMs,
    totalMs: round(vacuumMs + checkMs, 3),
  };
}

function restoreCheckMode(options) {
  const dbPath = options.db;
  if (!existsSync(dbPath)) throw new Error(`待恢复库不存在：${dbPath}`);
  const started = performance.now();
  const openStarted = performance.now();
  const db = openDatabase(dbPath);
  const openMs = round(performance.now() - openStarted, 3);
  const checkStarted = performance.now();
  const report = db.prepare("PRAGMA quick_check").all();
  const quickCheckMs = round(performance.now() - checkStarted, 3);
  const ok = report.length === 1 && String(report[0].quick_check).toLowerCase() === "ok";
  const migrateStarted = performance.now();
  const applied = applyMigrations(db, repoRoot);
  const migrateMs = round(performance.now() - migrateStarted, 3);
  db.close();
  return {
    db: dbPath,
    ok,
    openMs,
    quickCheckMs,
    migrateMs,
    appliedMigrations: applied,
    totalMs: round(performance.now() - started, 3),
  };
}

function retentionMode(options) {
  const dbPath = options.db;
  const days = Number(options.days ?? 30);
  const maxRows = options["max-rows"] !== undefined ? Number(options["max-rows"]) : 1_000_000;
  if (!existsSync(dbPath)) throw new Error(`库不存在：${dbPath}`);
  const db = openDatabase(dbPath);
  const cutoff = nowMs() - days * 24 * 60 * 60 * 1000;
  const fileBytesBefore = statSync(dbPath).size;
  const placeholders = RETENTION_TYPES.map(() => "?").join(", ");
  const countSql = `SELECT COUNT(*) AS count FROM events WHERE type IN (${placeholders}) AND ts < ?`;
  const eligible = Number(db.prepare(countSql).get(...RETENTION_TYPES, cutoff).count);
  // 选批使用 `idx_events_type_ts` 覆盖索引顺序（不排序 rowid）：删除集合与
  // ADR-021 产品口径一致，仅批顺序不同。产品 `select_retention_batch` 的
  // `ORDER BY rowid` 在 5GB 上会走 TEMP B-TREE（O(N²)），已记入 ADR-039 风险。
  const selectStmt = db.prepare(
    `SELECT rowid FROM events WHERE type IN (${placeholders}) AND ts < ? LIMIT 5000`,
  );
  const deleteStmt = db.prepare("DELETE FROM events WHERE rowid = ?");
  const batchTimes = [];
  let deleted = 0;
  let batches = 0;
  const started = performance.now();
  const limit = Number.isFinite(maxRows) && maxRows > 0 ? Math.min(maxRows, eligible) : eligible;
  while (deleted < limit) {
    const batchStarted = performance.now();
    const rowids = selectStmt.all(...RETENTION_TYPES, cutoff).map((row) => Number(row.rowid));
    if (rowids.length === 0) break;
    db.exec("BEGIN");
    try {
      for (const rowid of rowids) deleteStmt.run(rowid);
      db.exec("COMMIT");
    } catch (error) {
      try {
        db.exec("ROLLBACK");
      } catch {
        // 已回滚。
      }
      throw error;
    }
    batchTimes.push(performance.now() - batchStarted);
    deleted += rowids.length;
    batches += 1;
    if (batches % 50 === 0) {
      console.error(`[m10-08-worker] retention batches=${batches} deleted=${deleted}/${limit}`);
    }
  }
  const deleteMs = round(performance.now() - started, 3);
  const checkpointStarted = performance.now();
  db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
  const checkpointMs = round(performance.now() - checkpointStarted, 3);
  const pageCount = Number(db.prepare("PRAGMA page_count").get().page_count);
  const freelist = Number(db.prepare("PRAGMA freelist_count").get().freelist_count);
  const pageSize = Number(db.prepare("PRAGMA page_size").get().page_size);
  db.close();
  const stats = summarize(batchTimes);
  const liveBytes = (pageCount - freelist) * pageSize;
  const freedBytes = Math.max(0, fileBytesBefore - liveBytes);
  const freedPerRow = deleted > 0 ? freedBytes / deleted : 0;
  const projectedLiveBytes = Math.max(0, Math.round(fileBytesBefore - freedPerRow * eligible));
  const perRowMs = deleted > 0 ? deleteMs / deleted : 0;
  return {
    db: dbPath,
    retentionDays: days,
    cutoff,
    eligible,
    sampleRows: deleted,
    sampleComplete: deleted >= eligible,
    deleteMs,
    checkpointMs,
    batches: batchTimes.length,
    batchP95Ms: stats.p95,
    batchMaxMs: stats.max,
    rowsPerSecond: deleteMs > 0 ? Number(((deleted / deleteMs) * 1000).toFixed(1)) : 0,
    projectedFullDeleteMs: Math.round(perRowMs * eligible),
    pageCount,
    freelistCount: freelist,
    pageSize,
    fileBytesBefore,
    fileBytes: statSync(dbPath).size,
    liveBytes,
    projectedLiveBytes,
    selection: "covering index (type, ts); batch 5000; delete by rowid; no rowid sort",
  };
}

async function main() {
  if (workerData) {
    if (workerData.mode === "write") {
      await writeWorker(workerData);
      return;
    }
    if (workerData.mode === "read") {
      await readWorker(workerData);
      return;
    }
    throw new Error(`未知 workerData.mode：${workerData.mode}`);
  }

  const options = parseCli(process.argv.slice(2));
  const mode = options.mode;
  let result;
  switch (mode) {
    case "quick-check":
      result = quickCheckMode(options);
      break;
    case "backup":
      result = backupMode(options);
      break;
    case "restore-check":
      result = restoreCheckMode(options);
      break;
    case "retention":
      result = retentionMode(options);
      break;
    default:
      throw new Error(`未知 --mode：${mode}`);
  }
  console.log(JSON.stringify(result));
}

main().catch((error) => {
  console.error(`[m10-08-worker] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
