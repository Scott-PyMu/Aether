#!/usr/bin/env node
/**
 * M10-08 `events.db` 分库预案压测（ADR-038 D-038-1/D-038-2）。
 *
 * 覆盖：
 *  1. 10 并发写入（每会话 1 writer；D3 group commit 16ms/256 条）；
 *  2. 补读 `events_page` / `messages_page` / `last_seq`（10 并发）；
 *  3. 冷启动 `quick_check`；
 *  4. 备份（`VACUUM INTO`）/恢复（复制 + `quick_check` + 迁移）；
 *  5. 库增长采样（db+wal）与 30 天保留策略清理模拟；
 *  6. 单库 vs 按会话分片（N 个 DB）同数据集对照。
 *
 * 用法（完整 3 次运行由 run-full.mjs 编排）：
 *   node scripts/test/m10-08/bench-events.mjs --run-index 1
 *   node scripts/test/m10-08/bench-events.mjs --run-index 1 --label sharded --shards 8
 *
 * 安全：只读/写 `--dataset`（临时/`dist-out`）与 `--out`；不触碰用户库。
 */
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, rmSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { Worker } from "node:worker_threads";

import {
  GIB,
  THRESHOLDS,
  ensureDir,
  formatBytes,
  openDatabase,
  readJson,
  repoRootFrom,
  sessionIdFor,
  shardForSession,
  sizeSnapshot,
  sleep,
  summarize,
  writeJson,
} from "./lib.mjs";

const repoRoot = repoRootFrom(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const workerScript = path.join(here, "bench-worker.mjs");
const insertSql =
  "INSERT INTO events(id, session_id, run_id, runtime_id, seq, type, payload, ts, v) " +
  "VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1)";

function parseArgs(argv) {
  const options = {
    dataset: path.join(repoRoot, "dist-out", "m10-08", "dataset"),
    out: path.join(repoRoot, "dist-out", "m10-08", "results"),
    shardsDir: null,
    runIndex: 1,
    label: "single",
    shards: 8,
    writeSeconds: 600,
    readSeconds: 60,
    writers: 10,
    readers: 10,
    rate: 50,
    batchSize: 8,
    seed: null,
    quickCheckRuns: 3,
    skipQuickCheck: false,
    skipWrite: false,
    skipRead: false,
    skipBackup: false,
    skipRetention: false,
    keepArtifacts: false,
    copyDataset: true,
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
      case "--dataset":
        options.dataset = path.resolve(repoRoot, next());
        break;
      case "--out":
        options.out = path.resolve(repoRoot, next());
        break;
      case "--shards-dir":
        options.shardsDir = path.resolve(repoRoot, next());
        break;
      case "--run-index":
        options.runIndex = Number(next());
        break;
      case "--label":
        options.label = next();
        break;
      case "--shards":
        options.shards = Number(next());
        break;
      case "--write-seconds":
        options.writeSeconds = Number(next());
        break;
      case "--read-seconds":
        options.readSeconds = Number(next());
        break;
      case "--writers":
        options.writers = Number(next());
        break;
      case "--readers":
        options.readers = Number(next());
        break;
      case "--rate":
        options.rate = Number(next());
        break;
      case "--batch-size":
        options.batchSize = Number(next());
        break;
      case "--seed":
        options.seed = Number(next());
        break;
      case "--quick-check-runs":
        options.quickCheckRuns = Number(next());
        break;
      case "--skip-quick-check":
        options.skipQuickCheck = true;
        break;
      case "--skip-write":
        options.skipWrite = true;
        break;
      case "--skip-read":
        options.skipRead = true;
        break;
      case "--skip-backup":
        options.skipBackup = true;
        break;
      case "--skip-retention":
        options.skipRetention = true;
        break;
      case "--keep-artifacts":
        options.keepArtifacts = true;
        break;
      case "--no-copy":
        options.copyDataset = false;
        break;
      case "--quiet":
        options.quiet = true;
        break;
      case "--help":
      case "-h":
        console.log(
          "用法：node scripts/test/m10-08/bench-events.mjs [--run-index N] [--label single|sharded] " +
            "[--write-seconds N] [--read-seconds N] [--writers N] [--readers N] [--rate N] " +
            "[--skip-write] [--skip-read] [--skip-backup] [--skip-retention]",
        );
        process.exit(0);
        break;
      default:
        throw new Error(`未知参数：${arg}`);
    }
  }
  if (!["single", "sharded"].includes(options.label)) throw new Error("--label 必须是 single|sharded");
  if (!Number.isInteger(options.runIndex) || options.runIndex < 1) throw new Error("--run-index 必须 ≥1");
  if (!Number.isInteger(options.shards) || options.shards < 2) throw new Error("--shards 必须 ≥2");
  if (!Number.isInteger(options.writers) || options.writers < 1) throw new Error("--writers 必须 ≥1");
  if (!Number.isInteger(options.readers) || options.readers < 1) throw new Error("--readers 必须 ≥1");
  if (!Number.isInteger(options.batchSize) || options.batchSize < 1) throw new Error("--batch-size 必须 ≥1");
  if (!Number.isFinite(options.rate) || options.rate <= 0) throw new Error("--rate 必须为正数");
  return options;
}

function log(options, message) {
  if (!options.quiet) console.log(`[m10-08-bench] ${message}`);
}

function rel(filePath) {
  const relative = path.relative(repoRoot, filePath);
  if (relative.startsWith("..") || path.isAbsolute(relative)) {
    return `<external>/${path.basename(filePath)}`;
  }
  return relative.replaceAll("\\", "/");
}

function runChild(args, { timeoutMs = 0 } = {}) {
  const result = spawnSync(process.execPath, [workerScript, ...args], {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    timeout: timeoutMs,
  });
  if (result.error) throw new Error(`子进程失败：${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`子进程退出码 ${result.status}：${(result.stderr || result.stdout || "").slice(-2000)}`);
  }
  const line = (result.stdout ?? "").trim().split(/\r?\n/).filter(Boolean).at(-1);
  if (!line) throw new Error("子进程无 JSON 输出");
  try {
    return JSON.parse(line);
  } catch (error) {
    throw new Error(`子进程输出不是 JSON：${line.slice(0, 500)}（${error.message}）`);
  }
}

function readSessionIds(dbPath) {
  const db = openDatabase(dbPath);
  const rows = db.prepare("SELECT id FROM sessions ORDER BY id").all();
  db.close();
  return rows.map((row) => String(row.id));
}

function quickCheckPhase(options, dbPaths) {
  if (options.skipQuickCheck) return { skipped: true };
  const runs = [];
  for (let run = 0; run < options.quickCheckRuns; run += 1) {
    const perDb = [];
    for (const dbPath of dbPaths) {
      log(options, `quick_check ${run + 1}/${options.quickCheckRuns}：${rel(dbPath)}`);
      const entry = runChild(["--mode", "quick-check", "--db", dbPath]);
      perDb.push({ ...entry, db: rel(dbPath) });
    }
    runs.push({
      run: run + 1,
      ok: perDb.every((entry) => entry.ok),
      totalMs: round(perDb.reduce((sum, entry) => sum + entry.totalMs, 0)),
      maxDbMs: round(Math.max(...perDb.map((entry) => entry.totalMs))),
      perDb,
    });
  }
  const cold = runs[0];
  const totals = runs.map((entry) => entry.totalMs);
  return {
    runs,
    coldMs: cold.totalMs,
    coldMaxDbMs: cold.maxDbMs,
    medianMs: summarize(totals).p50,
    maxMs: summarize(totals).max,
    ok: runs.every((entry) => entry.ok),
  };
}

function round(value, digits = 3) {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

/**
 * 10 并发写入：worker 仅产生事件；父线程按 D3 group commit（16ms/256 条）
 * 单写连接提交后 ack，worker 延迟 = 提交完成 - 提交发起（含排队）。
 */
async function writePhase(options, dbPaths, sessionIds) {
  if (options.skipWrite) return { skipped: true };
  if (sessionIds.length < options.writers) {
    throw new Error(`writers=${options.writers} 超过数据集会话数=${sessionIds.length}`);
  }
  const dbs = dbPaths.map((dbPath) => openDatabase(dbPath));
  const statements = dbs.map((db) => db.prepare(insertSql));
  const pending = dbPaths.map(() => []);
  const windowStartedMs = dbPaths.map(() => 0);
  const commitSamples = dbPaths.map(() => []);
  const dbLabels = dbPaths.map((dbPath) => rel(dbPath));

  const latencies = [];
  const wakeDelays = [];
  const ackSamples = [];
  const commitRecords = [];
  let totalEvents = 0;
  const startedAt = Date.now();
  const startAt = startedAt + 500;
  const deadline = startAt + Math.round(options.writeSeconds * 1000);
  const workers = [];
  let activeWorkers = options.writers;
  let writeError = null;

  const growthSamples = [];
  const mainDbPath = dbPaths[0];
  const sampleGrowth = () => {
    const snapshot = sizeSnapshot(mainDbPath);
    growthSamples.push({ atMs: Date.now() - startedAt, ...snapshot });
  };
  sampleGrowth();
  const growthTimer = setInterval(sampleGrowth, 10_000);
  if (typeof growthTimer.unref === "function") growthTimer.unref();

  const commitShard = (shard) => {
    const batches = pending[shard].splice(0);
    if (batches.length === 0) return;
    const events = batches.flatMap((batch) => batch.events);
    const commitStarted = performance.now();
    let transactionStarted = false;
    try {
      dbs[shard].exec("BEGIN");
      transactionStarted = true;
      for (const event of events) {
        statements[shard].run(
          event.id,
          event.sessionId,
          event.runId,
          event.runtimeId,
          event.seq,
          event.type,
          event.payload,
          event.ts,
        );
      }
      dbs[shard].exec("COMMIT");
    } catch (error) {
      if (transactionStarted) {
        try {
          dbs[shard].exec("ROLLBACK");
        } catch {
          // 事务可能已因错误回滚。
        }
      }
      writeError = error;
      for (const worker of workers) void worker.terminate();
      return;
    }
    const commitMs = performance.now() - commitStarted;
    const at = Date.now();
    commitSamples[shard].push(commitMs);
    ackSamples.push(at - batches[0].sentAt);
    commitRecords.push({ shard, entries: events.length, commitMs: round(commitMs), atMs: at - startedAt });
    totalEvents += events.length;
    windowStartedMs[shard] = 0;
    for (const batch of batches) {
      const view = new Int32Array(batch.shared);
      Atomics.store(view, 2, at - startedAt);
      Atomics.store(view, 1, batch.ackValue);
      Atomics.notify(view, 1);
    }
  };

  for (let index = 0; index < options.writers; index += 1) {
    const sessionId = sessionIds[index];
    const shard = options.label === "sharded" ? shardForSession(sessionId, dbPaths.length) : 0;
    const shared = new SharedArrayBuffer(12);
    const worker = new Worker(workerScript, {
      workerData: {
        mode: "write",
        shared,
        dbPath: dbPaths[shard],
        sessionId,
        sessionIndex: index,
        batchSize: options.batchSize,
        ratePerWriter: options.rate,
        writers: options.writers,
        seed: options.seed,
        startedAt,
        startAt,
        deadline,
      },
    });
    worker.on("message", (message) => {
      if (message.type === "batch") {
        if (pending[shard].length === 0) windowStartedMs[shard] = Date.now();
        pending[shard].push({
          shared,
          ackValue: message.ackValue,
          events: message.events,
          sentAt: message.sentAt,
        });
        return;
      }
      if (message.type === "done") {
        latencies.push(...message.latencies);
        wakeDelays.push(...message.wakeDelays);
        activeWorkers -= 1;
      }
    });
    worker.on("error", (error) => {
      writeError = error;
      for (const entry of workers) void entry.terminate();
    });
    workers.push(worker);
  }

  log(
    options,
    `写入阶段：${options.writers} writers / ${options.rate}/s/writer / ${options.writeSeconds}s / ` +
      `${options.label === "sharded" ? `${dbPaths.length} shards` : "single db"}`,
  );
  while (activeWorkers > 0 || pending.some((entries) => entries.length > 0)) {
    if (writeError) break;
    await new Promise((resolve) => setImmediate(resolve));
    for (let shard = 0; shard < dbPaths.length; shard += 1) {
      const pendingEvents = pending[shard].reduce((sum, batch) => sum + batch.events.length, 0);
      if (pendingEvents === 0) continue;
      const canCommit =
        pendingEvents >= 256 ||
        Date.now() - windowStartedMs[shard] >= 16 ||
        activeWorkers === 0;
      if (canCommit) commitShard(shard);
    }
  }
  // 收尾：drain 残余批次（worker 已全部结束时）。
  if (!writeError) {
    for (let shard = 0; shard < dbPaths.length; shard += 1) commitShard(shard);
  }
  clearInterval(growthTimer);
  sampleGrowth();
  for (const worker of workers) await worker.terminate();
  for (const db of dbs) db.close();
  if (writeError) throw writeError;

  const elapsedSeconds = (Date.now() - startedAt) / 1000;
  const growthFirst = growthSamples[0];
  const growthLast = growthSamples.at(-1);
  const growthBytesPerHour =
    growthSamples.length >= 2
      ? round(((growthLast.total - growthFirst.total) / Math.max(1, growthLast.atMs - growthFirst.atMs)) * 3_600_000)
      : 0;

  return {
    startedAt: new Date(startedAt).toISOString(),
    seconds: round(elapsedSeconds),
    writers: options.writers,
    ratePerWriter: options.rate,
    batchSize: options.batchSize,
    totalEvents,
    eventsPerSecond: round(totalEvents / Math.max(1, elapsedSeconds)),
    latencyMs: summarize(latencies),
    latencyUnit: `每 ${options.batchSize} 条 group-commit 批次（worker 视角，含排队）`,
    commitMs: summarize(commitSamples.flat()),
    ackMs: summarize(ackSamples),
    wakeMs: summarize(wakeDelays),
    commitBatches: commitRecords.length,
    shards: options.label === "sharded" ? dbPaths.length : 1,
    perShard: dbLabels.map((label, shard) => ({
      db: label,
      events: commitRecords.filter((entry) => entry.shard === shard).reduce((sum, entry) => sum + entry.entries, 0),
      commitBatches: commitRecords.filter((entry) => entry.shard === shard).length,
    })),
    growth: {
      sampleIntervalMs: 10_000,
      points: growthSamples,
      bytesPerHour: growthBytesPerHour,
      bytesPerEvent: totalEvents > 0 ? round((growthLast.total - growthFirst.total) / totalEvents) : 0,
    },
  };
}

function readPhase(options, shards) {
  if (options.skipRead) return Promise.resolve({ skipped: true });
  const startAt = Date.now() + 500;
  const deadline = startAt + Math.round(options.readSeconds * 1000);
  const all = { eventsPage: [], messagesPage: [], lastSeq: [] };
  let active = options.readers;
  const runStarted = Date.now();
  return new Promise((resolve, reject) => {
    for (let index = 0; index < options.readers; index += 1) {
      const worker = new Worker(workerScript, {
        workerData: { mode: "read", shards, startAt, deadline },
      });
      worker.on("message", (message) => {
        if (message.type !== "done") return;
        all.eventsPage.push(...message.samples.eventsPage);
        all.messagesPage.push(...message.samples.messagesPage);
        all.lastSeq.push(...message.samples.lastSeq);
      });
      worker.on("error", reject);
      worker.on("exit", () => {
        active -= 1;
        if (active === 0) {
          resolve({
            seconds: round((Date.now() - runStarted) / 1000),
            readers: options.readers,
            shards: shards.length,
            samples: {
              eventsPage: summarize(all.eventsPage),
              messagesPage: summarize(all.messagesPage),
              lastSeq: summarize(all.lastSeq),
            },
          });
        }
      });
    }
    log(options, `补读阶段：${options.readers} readers / ${options.readSeconds}s / ${shards.length} 库`);
  });
}

function backupRestorePhase(options, dbPaths) {
  if (options.skipBackup) return { skipped: true };
  const workDir = path.join(options.out, `artifacts-run${options.runIndex}`);
  rmSync(workDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  ensureDir(path.join(workDir, "backup"));
  ensureDir(path.join(workDir, "restore"));

  const backups = [];
  const restores = [];
  for (const dbPath of dbPaths) {
    const name = path.basename(dbPath);
    const backupPath = path.join(workDir, "backup", name);
    log(options, `备份：${rel(dbPath)} → ${rel(backupPath)}`);
    const backup = runChild(["--mode", "backup", "--db", dbPath, "--out", backupPath]);
    backup.source = rel(backup.source);
    backup.backup = rel(backup.backup);
    backups.push(backup);

    const restorePath = path.join(workDir, "restore", name);
    const copyStarted = performance.now();
    copyFileSync(backupPath, restorePath);
    const copyMs = performance.now() - copyStarted;
    log(options, `恢复校验：${rel(restorePath)}`);
    const check = runChild(["--mode", "restore-check", "--db", restorePath]);
    restores.push({
      db: rel(dbPath),
      copyMs: round(copyMs),
      check,
      totalMs: round(copyMs + check.totalMs),
      bytes: statSync(restorePath).size,
    });
  }

  const backupTotalMs = backups.reduce((sum, entry) => sum + entry.vacuumMs + entry.verifyQuickCheckMs, 0);
  const restoreTotalMs = restores.reduce((sum, entry) => sum + entry.totalMs, 0);
  const retentionSource = path.join(workDir, "backup", path.basename(dbPaths[0]));
  const result = {
    backups,
    restores,
    backupMs: round(backupTotalMs),
    backupMaxDbMs: round(Math.max(...backups.map((entry) => entry.vacuumMs + entry.verifyQuickCheckMs))),
    restoreMs: round(restoreTotalMs),
    restoreMaxDbMs: round(Math.max(...restores.map((entry) => entry.totalMs))),
    totalBytes: backups.reduce((sum, entry) => sum + entry.backupBytes, 0),
    ok: backups.every((entry) => entry.ok) && restores.every((entry) => entry.check.ok),
    artifacts: options.keepArtifacts ? rel(workDir) : null,
    retentionSource,
  };
  return result;
}

function retentionPhase(options, backupPath, dataset, extraRows) {
  if (options.skipRetention) return { skipped: true };
  const workDir = path.join(options.out, `artifacts-run${options.runIndex}`);
  const retentionDir = path.join(workDir, "retention");
  ensureDir(retentionDir);
  const target = path.join(retentionDir, path.basename(backupPath));
  const copyStarted = performance.now();
  copyFileSync(backupPath, target);
  const copyMs = performance.now() - copyStarted;
  log(options, `保留策略清理模拟（30 天，消息/用量/日志先归档后删除；采样 50,000 行）：${rel(target)}`);
  const result = runChild(["--mode", "retention", "--db", target, "--days", "30", "--max-rows", "50000"], {
    timeoutMs: 0,
  });
  const totalRows = dataset.rows + (extraRows ?? 0);
  const retentionShare = totalRows > 0 ? result.eligible / totalRows : 0;
  // 采样删除不按比例释放页（页面复用/高水位），物理文件不收缩；
  // 12 个月投影按类型分布的行数占比保守估算「保留后 live rows」，再折算字节。
  const deletableRows = ["message.delta", "usage", "log"].reduce(
    (sum, type) => sum + (dataset.typeDistribution?.[type]?.rows ?? 0),
    0,
  );
  const nonDeletableRows = Math.max(0, dataset.rows - deletableRows);
  const projectedRows = nonDeletableRows + deletableRows * (30 / 365);
  const projectedRetainedBytes = Math.round((dataset.bytes.total * projectedRows) / Math.max(1, dataset.rows));
  const resultWithProjection = {
    ...result,
    db: rel(result.db),
    copyMs: round(copyMs),
    retentionShareOfRows: round(retentionShare, 6),
    sampleLiveBytes: result.liveBytes,
    analyticProjectedBytes: projectedRetainedBytes,
    projectedRetainedBytes,
    projectedRetainedGiB: round(projectedRetainedBytes / GIB, 3),
    projectedFullDeleteMinutes: round((result.projectedFullDeleteMs ?? 0) / 60_000, 1),
    projectedFullDeleteHours: round((result.projectedFullDeleteMs ?? 0) / 3_600_000, 2),
    belowFiveGiB: projectedRetainedBytes < THRESHOLDS.databaseBytes,
  };
  if (!options.keepArtifacts) {
    rmSync(retentionDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
  return resultWithProjection;
}

function cleanupArtifacts(options, runDataDir) {
  if (!options.keepArtifacts) {
    const workDir = path.join(options.out, `artifacts-run${options.runIndex}`);
    rmSync(workDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    if (runDataDir) {
      rmSync(runDataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    }
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const datasetPath = path.join(options.dataset, "aether.db");
  const manifestPath = path.join(options.dataset, "dataset.json");
  if (!existsSync(datasetPath) || !existsSync(manifestPath)) {
    throw new Error(`数据集不存在：${options.dataset}（先运行 gen-events-dataset.mjs）`);
  }
  const dataset = readJson(manifestPath);
  if (options.seed === null) options.seed = dataset.seed;

  let dbPaths;
  let shardsManifest = null;
  if (options.label === "sharded") {
    const shardsDir = options.shardsDir ?? path.join(options.dataset, "..", "shards");
    const shardsPath = path.join(shardsDir, "shards.json");
    if (!existsSync(shardsPath)) {
      throw new Error(`分片数据集不存在：${shardsDir}（先运行 build-shards.mjs）`);
    }
    shardsManifest = readJson(shardsPath);
    if (shardsManifest.shards !== options.shards) {
      throw new Error(`分片数不一致：manifest=${shardsManifest.shards} / --shards=${options.shards}`);
    }
    dbPaths = shardsManifest.files.map((file) => path.join(shardsDir, file.db));
  } else {
    dbPaths = [datasetPath];
  }

  const outDir = ensureDir(options.out);
  const metricsPath = path.join(outDir, `metrics-run${options.runIndex}-${options.label}.json`);
  // 每次运行复制一份数据集，保证基准起点与 dataset.json 的 sha256/行数一致，
  // 写压测不污染源数据集（ADR-038 可重复性）。
  let runDataDir = null;
  let effectiveDbPaths = dbPaths;
  if (options.copyDataset) {
    runDataDir = path.join(outDir, `data-run${options.runIndex}-${options.label}`);
    rmSync(runDataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    ensureDir(runDataDir);
    effectiveDbPaths = dbPaths.map((dbPath) => {
      const target = path.join(runDataDir, path.basename(dbPath));
      copyFileSync(dbPath, target);
      return target;
    });
  }
  const sessionIds =
    options.label === "sharded"
      ? Array.from({ length: dataset.sessions }, (_, index) => sessionIdFor(index))
      : readSessionIds(effectiveDbPaths[0]);
  const readShards =
    options.label === "sharded"
      ? effectiveDbPaths.map((dbPath, shard) => ({
          dbPath,
          sessionIds: sessionIds.filter((sessionId) => shardForSession(sessionId, effectiveDbPaths.length) === shard),
        }))
      : [{ dbPath: effectiveDbPaths[0], sessionIds }];
  const startedAt = new Date().toISOString();
  const failures = [];

  log(
    options,
    `run ${options.runIndex}（${options.label}）：数据集 ${dataset.rows.toLocaleString()} 行 / ` +
      `${formatBytes(dataset.bytes.total)} / ${options.writeSeconds}s 写入窗口` +
      `${runDataDir ? ` / 副本 ${rel(runDataDir)}` : " / 原地写"}`,
  );

  const quickCheck = quickCheckPhase(options, effectiveDbPaths);
  if (quickCheck.ok === false) failures.push("quick_check 失败");

  let write = { skipped: true };
  try {
    write = await writePhase(options, effectiveDbPaths, sessionIds);
  } catch (error) {
    failures.push(`写入失败：${error.message}`);
    throw error;
  }

  const read = await readPhase(options, readShards);
  const backupRestore = backupRestorePhase(options, effectiveDbPaths);
  if (!backupRestore.skipped && !backupRestore.ok) failures.push("备份/恢复校验失败");
  const retention =
    options.label === "single" && backupRestore.retentionSource
      ? retentionPhase(options, backupRestore.retentionSource, dataset, write.totalEvents ?? 0)
      : { skipped: true };

  const writeP95 = write.latencyMs?.p95 ?? 0;
  const readP95 = Math.max(
    read.samples?.eventsPage?.p95 ?? 0,
    read.samples?.messagesPage?.p95 ?? 0,
    read.samples?.lastSeq?.p95 ?? 0,
  );
  const thresholds = {
    writeP95Ms: THRESHOLDS.writeP95Ms,
    readP95Ms: THRESHOLDS.readP95Ms,
    quickCheckMs: THRESHOLDS.quickCheckMs,
    backupMs: THRESHOLDS.backupMs,
    restoreMs: THRESHOLDS.restoreMs,
    databaseBytes: THRESHOLDS.databaseBytes,
  };
  const violations = [];
  if (!write.skipped && writeP95 > THRESHOLDS.writeP95Ms) violations.push(`writeP95=${writeP95}ms`);
  if (!read.skipped && readP95 >= THRESHOLDS.readP95Ms) violations.push(`readP95=${readP95}ms`);
  if (!quickCheck.skipped && quickCheck.coldMs > THRESHOLDS.quickCheckMs) {
    violations.push(`quickCheckCold=${quickCheck.coldMs}ms`);
  }
  if (!backupRestore.skipped && backupRestore.backupMs > THRESHOLDS.backupMs) {
    violations.push(`backup=${backupRestore.backupMs}ms`);
  }
  if (!backupRestore.skipped && backupRestore.restoreMs > THRESHOLDS.restoreMs) {
    violations.push(`restore=${backupRestore.restoreMs}ms`);
  }
  if (failures.length > 0) violations.push(...failures);

  const metrics = {
    task: "M10-08",
    adr: "ADR-038",
    label: options.label,
    runIndex: options.runIndex,
    startedAt,
    completedAt: new Date().toISOString(),
    dataset: {
      dir: rel(options.dataset),
      rows: dataset.rows,
      sessions: dataset.sessions,
      sha256: dataset.sha256,
      bytes: dataset.bytes,
      typeDistribution: dataset.typeDistribution,
    },
    machine: dataset.machine,
    config: {
      writeSeconds: options.writeSeconds,
      readSeconds: options.readSeconds,
      writers: options.writers,
      readers: options.readers,
      ratePerWriter: options.rate,
      batchSize: options.batchSize,
      shards: options.label === "sharded" ? effectiveDbPaths.length : 1,
      seed: options.seed,
      datasetCopied: options.copyDataset,
    },
    quickCheck,
    write,
    read,
    backupRestore,
    retention,
    thresholds,
    violations,
    failures,
    ok: violations.length === 0,
    machineNote: dataset.machine?.diskBusType
      ? `volume=${dataset.machine.volume} / disk=${dataset.machine.diskModel} / bus=${dataset.machine.diskBusType}`
      : `volume=${dataset.machine?.volume ?? "unknown"}`,
  };
  writeJson(metricsPath, metrics);
  cleanupArtifacts(options, runDataDir);

  log(
    options,
    `完成：write p95=${metrics.write.latencyMs?.p95 ?? "-"}ms / read p95=${readP95}ms / ` +
      `quick_check cold=${quickCheck.coldMs ?? "-"}ms / backup=${backupRestore.backupMs ?? "-"}ms / ` +
      `restore=${backupRestore.restoreMs ?? "-"}ms / ${metrics.ok ? "OK" : "越线"}`,
  );
  console.log(JSON.stringify({ metrics: metricsPath, ok: metrics.ok, violations }, null, 2));
}

main().catch((error) => {
  console.error(`[m10-08-bench] FAIL：${error.stack ?? error.message}`);
  process.exit(1);
});
