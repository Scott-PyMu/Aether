#!/usr/bin/env node
/**
 * M10-08 数据集构造（ADR-038 D-038-1）。
 *
 * 目标：`events` 表接近 5 GiB（db+wal 合计）、≥ 20,000,000 行，混合类型，
 * 每会话 `seq` 连续、`UNIQUE(session_id, seq)` 成立。固定 seed，输出
 * `dataset.json`（行数/类型分布/字节数/sha256/机器信息）。
 *
 * 用法：
 *   node scripts/test/m10-08/gen-events-dataset.mjs
 *   node scripts/test/m10-08/gen-events-dataset.mjs --out dist-out/m10-08/dataset --force
 *   node scripts/test/m10-08/gen-events-dataset.mjs --target-gib 0.125 --min-rows 500000 \
 *     --sessions 10 --out scripts/test/.tmp/m10-08-smoke/dataset
 *
 * 安全：只写 `--out` 指定目录；不触碰用户库；不引入外部凭证。
 */
import { existsSync, readFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import {
  DATASET_DEFAULTS,
  EVENT_INSERT_SQL,
  EVENT_TYPES,
  GIB,
  MIB,
  applyMigrations,
  ensureDir,
  formatBytes,
  listMigrationFiles,
  makePayload,
  machineInfo,
  mulberry32,
  openDatabase,
  pickEventType,
  repoRootFrom,
  sessionIdFor,
  sha256File,
  sizeSnapshot,
  sqliteVersion,
  writeJson,
} from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);

function parseArgs(argv) {
  const options = {
    out: path.join(repoRoot, "dist-out", "m10-08", "dataset"),
    targetBytes: DATASET_DEFAULTS.targetBytes,
    minRows: DATASET_DEFAULTS.minRows,
    maxRows: DATASET_DEFAULTS.maxRows,
    sessions: DATASET_DEFAULTS.sessions,
    seed: DATASET_DEFAULTS.seed,
    force: false,
    quiet: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      index += 1;
      if (index >= argv.length) throw new Error(`${arg} 缺少参数`);
      return argv[index];
    };
    switch (arg) {
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--target-gib":
        options.targetBytes = Math.round(Number(next()) * GIB);
        break;
      case "--target-mib":
        options.targetBytes = Math.round(Number(next()) * MIB);
        break;
      case "--min-rows":
        options.minRows = Number(next());
        break;
      case "--max-rows":
        options.maxRows = Number(next());
        break;
      case "--sessions":
        options.sessions = Number(next());
        break;
      case "--seed":
        options.seed = Number(next());
        break;
      case "--force":
        options.force = true;
        break;
      case "--quiet":
        options.quiet = true;
        break;
      case "--help":
      case "-h":
        console.log(
          "用法：node scripts/test/m10-08/gen-events-dataset.mjs [--out DIR] [--target-gib N] " +
            "[--min-rows N] [--sessions N] [--seed N] [--force]",
        );
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!Number.isFinite(options.targetBytes) || options.targetBytes <= 0) {
    throw new Error("--target-gib/--target-mib 必须为正数");
  }
  if (!Number.isInteger(options.minRows) || options.minRows < 0) {
    throw new Error("--min-rows 必须为非负整数");
  }
  if (!Number.isInteger(options.maxRows) || options.maxRows < options.minRows) {
    throw new Error("--max-rows 必须 ≥ --min-rows");
  }
  if (!Number.isInteger(options.sessions) || options.sessions < 2) {
    throw new Error("--sessions 必须 ≥ 2");
  }
  return options;
}

function log(options, message) {
  if (!options.quiet) console.log(`[m10-08-gen] ${message}`);
}

function seedReferenceRows(db, options, now) {
  const runtimes = [
    {
      id: "claude-code",
      name: "Claude Code",
      kind: "claude-code",
      version: "1.0.0",
    },
    {
      id: "codex",
      name: "Codex",
      kind: "codex",
      version: "1.0.0",
    },
    {
      id: "deepseek-harness",
      name: "DeepSeek Harness",
      kind: "deepseek-harness",
      version: "0.2.1-rc.2",
    },
  ];
  const insertRuntime = db.prepare(
    "INSERT INTO runtimes(id, name, kind, version, protocol, capabilities, config, status, enabled, created_at, updated_at) " +
      "VALUES (?, ?, ?, ?, '1.0', '[]', '{}', 'ready', 1, ?, ?)",
  );
  for (const runtime of runtimes) {
    insertRuntime.run(runtime.id, runtime.name, runtime.kind, runtime.version, now, now);
  }

  db.prepare(
    "INSERT INTO workspaces(id, name, root_path, memory_files, created_at, updated_at) VALUES (?, ?, ?, '[]', ?, ?)",
  ).run("ws-m10-08", "M10-08 benchmark", "C:/aether-m10-08-workspace", now, now);

  const insertSession = db.prepare(
    "INSERT INTO sessions(id, runtime_id, workspace_id, title, status, model, config, token_usage, created_at, updated_at) " +
      "VALUES (?, ?, ?, ?, 'idle', ?, '{}', '{}', ?, ?)",
  );
  const insertMessage = db.prepare(
    "INSERT INTO messages(id, session_id, role, content, seq, created_at) VALUES (?, ?, 'assistant', ?, ?, ?)",
  );
  const sessions = [];
  for (let index = 0; index < options.sessions; index += 1) {
    const id = sessionIdFor(index);
    const runtime = runtimes[index % runtimes.length];
    sessions.push(id);
    insertSession.run(id, runtime.id, "ws-m10-08", `Benchmark session ${index}`, "mock-model", now, now);
    for (let seq = 1; seq <= 200; seq += 1) {
      insertMessage.run(
        `msg-${String(index).padStart(4, "0")}-${String(seq).padStart(4, "0")}`,
        id,
        `synthetic message ${index}/${seq} ${"z".repeat(120)}`,
        seq,
        now,
      );
    }
  }
  return sessions;
}

function currentBytes(dbPath) {
  return sizeSnapshot(dbPath).total;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const startedAt = Date.now();
  const dbPath = path.join(options.out, "aether.db");
  const manifestPath = path.join(options.out, "dataset.json");

  if (existsSync(manifestPath) && !options.force) {
    const existing = JSON.parse(readFileSync(manifestPath, "utf8"));
    log(
      options,
      `已存在数据集（${existing.rows} 行 / ${formatBytes(existing.bytes?.total ?? 0)}）；如需重建加 --force`,
    );
    console.log(JSON.stringify({ reused: true, manifest: manifestPath, ...existing }, null, 2));
    return;
  }
  if (existsSync(options.out) && options.force) {
    rmSync(options.out, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
  ensureDir(options.out);

  const now = Date.now();
  const random = mulberry32(options.seed);
  const typeCounts = Object.fromEntries(EVENT_TYPES.map((entry) => [entry.type, 0]));
  const typePayloadBytes = Object.fromEntries(EVENT_TYPES.map((entry) => [entry.type, 0]));
  let payloadBytesTotal = 0;

  log(options, `创建 ${dbPath}`);
  const db = openDatabase(dbPath);
  // 批量构造阶段关闭 WAL：避免 5GiB 数据集在生成期产生巨量 -wal；
  // 生成结束后恢复 D3 的 WAL + NORMAL 并 checkpoint（终态与产品一致）。
  db.exec("PRAGMA journal_mode = OFF");
  db.exec("PRAGMA synchronous = OFF");
  db.exec("PRAGMA cache_size = -65536");
  applyMigrations(db, repoRoot);
  log(options, `迁移 ${listMigrationFiles(repoRoot).length} 个版本已应用（schema 终态 v13）`);

  db.exec("BEGIN");
  const sessions = seedReferenceRows(db, options, now);
  db.exec("COMMIT");

  // 索引最后统一构建：不改变终态 DDL，仅缩短 2000 万行构造时间。
  db.exec("DROP INDEX IF EXISTS idx_events_session_seq_uq");
  db.exec("DROP INDEX IF EXISTS idx_events_type_ts");

  const insert = db.prepare(EVENT_INSERT_SQL);
  const seqs = new Int32Array(options.sessions);
  let rows = 0;
  let lastReport = 0;
  let bytesTotal = currentBytes(dbPath);
  const targetTableBytes = Math.round(options.targetBytes * 0.62);
  const progress = (force) => {
    if (force || rows - lastReport >= 500_000) {
      lastReport = rows;
      log(
        options,
        `构造中：${rows.toLocaleString()} 行 / ${formatBytes(bytesTotal)}（目标 ${formatBytes(options.targetBytes)}）`,
      );
    }
  };

  const insertRows = (count) => {
    db.exec("BEGIN");
    for (let index = 0; index < count; index += 1) {
      const sessionIndex = Math.floor(random() * options.sessions);
      seqs[sessionIndex] += 1;
      const type = pickEventType(random);
      const payload = makePayload(type, rows, random);
      typeCounts[type] += 1;
      typePayloadBytes[type] += Buffer.byteLength(payload);
      payloadBytesTotal += Buffer.byteLength(payload);
      // ts 在最近 365 天内均匀分布，供 30 天保留策略清理模拟使用。
      const ts = now - Math.floor(random() * 365 * 24 * 60 * 60 * 1000);
      insert.run(
        `evt-${String(sessionIndex).padStart(4, "0")}-${String(seqs[sessionIndex]).padStart(10, "0")}`,
        sessions[sessionIndex],
        null,
        sessionIndex % 3 === 0 ? "claude-code" : sessionIndex % 3 === 1 ? "codex" : "deepseek-harness",
        seqs[sessionIndex],
        type,
        payload,
        ts,
      );
      rows += 1;
    }
    db.exec("COMMIT");
  };

  // 阶段 A：表数据达到目标的一定比例（索引约占终态 35%–40%）。
  while ((rows < options.minRows || bytesTotal < targetTableBytes) && rows < options.maxRows) {
    insertRows(100_000);
    bytesTotal = currentBytes(dbPath);
    progress(false);
  }
  progress(true);

  log(options, "构建事件索引（UNIQUE(session_id, seq) + (type, ts)）…");
  const indexStarted = Date.now();
  db.exec("CREATE UNIQUE INDEX idx_events_session_seq_uq ON events(session_id, seq)");
  db.exec("CREATE INDEX idx_events_type_ts ON events(type, ts)");
  db.exec("ANALYZE");
  const indexMs = Date.now() - indexStarted;

  // 阶段 B：索引后仍显著低于目标时继续补行（小数据集复跑时通常不触发）。
  bytesTotal = currentBytes(dbPath);
  while (bytesTotal < options.targetBytes * 0.95 && rows < options.maxRows) {
    insertRows(50_000);
    bytesTotal = currentBytes(dbPath);
    progress(false);
  }
  progress(true);

  db.exec("PRAGMA journal_mode = WAL");
  db.exec("PRAGMA synchronous = NORMAL");
  db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
  const sqlite = sqliteVersion(db);
  db.close();

  const bytes = sizeSnapshot(dbPath);
  log(options, `计算 sha256（${formatBytes(bytes.total)}）…`);
  const hashStarted = Date.now();
  const sha256 = await sha256File(dbPath);
  const hashMs = Date.now() - hashStarted;

  const typeDistribution = {};
  for (const entry of EVENT_TYPES) {
    typeDistribution[entry.type] = {
      rows: typeCounts[entry.type],
      share: Number((typeCounts[entry.type] / rows).toFixed(6)),
      avgPayloadBytes: Number((typePayloadBytes[entry.type] / Math.max(1, typeCounts[entry.type])).toFixed(1)),
    };
  }

  const manifest = {
    task: "M10-08",
    adr: "ADR-038",
    datasetVersion: 1,
    seed: options.seed,
    generatedAt: new Date().toISOString(),
    schemaVersion: listMigrationFiles(repoRoot).at(-1)?.version ?? 0,
    sessions: options.sessions,
    messages: options.sessions * 200,
    rows,
    bytes,
    avgRowBytes: Number((bytes.total / rows).toFixed(2)),
    avgPayloadBytes: Number((payloadBytesTotal / rows).toFixed(2)),
    sha256,
    typeDistribution,
    generation: {
      totalMs: Date.now() - startedAt,
      indexMs,
      hashMs,
      targetBytes: options.targetBytes,
      minRows: options.minRows,
      maxRows: options.maxRows,
      payloadProfile: { deltaChars: 130, logChars: 110 },
    },
    sqliteVersion: sqlite,
    machine: machineInfo(options.out),
  };
  writeJson(manifestPath, manifest);
  log(
    options,
    `完成：${rows.toLocaleString()} 行 / ${formatBytes(bytes.total)} / sha256=${sha256.slice(0, 16)}… / ${(
      (Date.now() - startedAt) /
      1000
    ).toFixed(1)}s`,
  );
  console.log(JSON.stringify({ manifest: manifestPath, ...manifest }, null, 2));
}

main().catch((error) => {
  console.error(`[m10-08-gen] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
