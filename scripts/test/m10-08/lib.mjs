#!/usr/bin/env node
/**
 * M10-08 事件分库预案：共享常量/工具（ADR-038）。
 *
 * 本任务只验证、不实施分库：
 *  - 不新增 `events_*` 表 / 迁移 / 命令 / 设置键；
 *  - 数据集生成到临时/`dist-out` 目录，不触碰用户库；
 *  - 所有压测连接使用 D3 PRAGMA 全集（WAL / NORMAL / busy_timeout / cache_size 等）。
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  statfsSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
let sqliteModule = null;

function requireSqlite() {
  if (sqliteModule === null) {
    // 延迟加载：静态语法检查/无 SQLite 的环境不因 import 阶段失败。
    sqliteModule = require("node:sqlite");
  }
  return sqliteModule;
}

export const GIB = 1024 ** 3;
export const MIB = 1024 ** 2;

/** ADR-038 D-038-2 硬阈值（与 docs/M10-00-登记表.json `limits.sharding` 同口径）。 */
export const THRESHOLDS = Object.freeze({
  writeP95Ms: 50,
  readP95Ms: 150,
  quickCheckMs: 30_000,
  backupMs: 600_000,
  restoreMs: 900_000,
  databaseBytes: 5 * GIB,
  startupBudgetMs: 1_500,
  failures: 0,
});

/** ADR-038 D-038-1：目标数据集。 */
export const DATASET_DEFAULTS = Object.freeze({
  targetBytes: 5 * GIB,
  minRows: 20_000_000,
  maxRows: 30_000_000,
  sessions: 100,
  seed: 20_261_112,
});

/** 混合事件类型（权重与 M10-08 证据 §2 一致；message.delta 为大头）。 */
export const EVENT_TYPES = Object.freeze([
  { type: "message.delta", weight: 0.62 },
  { type: "usage", weight: 0.1 },
  { type: "log", weight: 0.08 },
  { type: "run.started", weight: 0.05 },
  { type: "run.completed", weight: 0.05 },
  { type: "message.completed", weight: 0.04 },
  { type: "permission.requested", weight: 0.02 },
  { type: "tool.call_started", weight: 0.02 },
  { type: "tool.call_completed", weight: 0.02 },
]);

/** 可被 ADR-021 保留策略清理的类型（D-038-4）。 */
export const RETENTION_TYPES = Object.freeze(["message.delta", "usage", "log"]);

export const EVENT_INSERT_SQL =
  "INSERT INTO events(id, session_id, run_id, runtime_id, seq, type, payload, ts, v) " +
  "VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1)";

export const EVENTS_PAGE_SQL =
  "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v " +
  "FROM events WHERE session_id = ?1 AND (?2 IS NULL OR seq > ?2) " +
  "ORDER BY seq ASC LIMIT ?3";

export const MESSAGES_PAGE_SQL =
  "SELECT id, session_id, run_id, client_msg_id, role, content, content_parts, tool_calls, " +
  "parent_message_id, seq, created_at FROM messages " +
  "WHERE session_id = ?1 AND (?2 IS NULL OR seq > ?2) ORDER BY seq ASC LIMIT ?3";

export const LAST_SEQ_SQL = "SELECT COALESCE(MAX(seq), 0) AS seq FROM events WHERE session_id = ?1";

/** 固定 seed 的 mulberry32（跨平台/跨 Node 小版本可复现）。 */
export function mulberry32(seed) {
  let state = seed | 0;
  return function next() {
    state = (state + 0x6d2b79f5) | 0;
    let value = Math.imul(state ^ (state >>> 15), 1 | state);
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
    return ((value ^ (value >>> 14)) >>> 0) / 4_294_967_296;
  };
}

export function pickEventType(random) {
  let cursor = random();
  for (const entry of EVENT_TYPES) {
    if (cursor < entry.weight) return entry.type;
    cursor -= entry.weight;
  }
  return "log";
}

/** 生成与类型匹配的 payload；`profile` 控制文本长度（用于把行宽校准到 ~256B）。 */
export function makePayload(type, index, random, profile = {}) {
  const deltaChars = profile.deltaChars ?? 130;
  const logChars = profile.logChars ?? 110;
  const noise = Math.floor(random() * 1_000_000).toString(36);
  switch (type) {
    case "message.delta":
      return JSON.stringify({ text: `${noise}-${"x".repeat(deltaChars)}` });
    case "usage":
      return JSON.stringify({ input_tokens: 1024 + (index % 4096), output_tokens: 256 + (index % 1024) });
    case "log":
      return JSON.stringify({ level: "info", message: `${noise}-${"y".repeat(logChars)}` });
    case "run.started":
      return JSON.stringify({ runId: `run-${noise}` });
    case "run.completed":
      return JSON.stringify({ runId: `run-${noise}`, usage: { input: 12, output: 34 } });
    case "message.completed":
      return JSON.stringify({ messageId: `msg-${noise}` });
    case "permission.requested":
      return JSON.stringify({ requestId: `req-${noise}`, resource: "fs.write", action: "write" });
    case "tool.call_started":
      return JSON.stringify({ toolCallId: `tool-${noise}`, name: "read_file" });
    case "tool.call_completed":
      return JSON.stringify({ toolCallId: `tool-${noise}`, status: "ok" });
    default:
      return JSON.stringify({ message: noise });
  }
}

export function sessionIdFor(index) {
  return `sess-${String(index).padStart(4, "0")}`;
}

export function sessionIndexFromId(sessionId) {
  const match = /^sess-(\d+)$/.exec(sessionId);
  return match ? Number(match[1]) : Number.NaN;
}

export function shardForSession(sessionId, shards) {
  const index = sessionIndexFromId(sessionId);
  if (!Number.isFinite(index)) throw new Error(`非法 session_id：${sessionId}`);
  return index % shards;
}

export function listMigrationFiles(repoRoot) {
  const dir = path.join(repoRoot, "migrations");
  return readdirSync(dir)
    .filter((name) => /^\d{4}_.+\.sql$/.test(name))
    .sort()
    .map((name) => {
      const bytes = readFileSync(path.join(dir, name));
      return {
        version: Number(name.slice(0, 4)),
        name,
        bytes,
        sql: bytes.toString("utf8"),
        checksum: createHash("sha256").update(bytes).digest("hex"),
      };
    });
}

/**
 * 以与 `aether-store::migration` 相同的口径建库/校验：
 * 单版本单事务、`schema_migrations(version, checksum, applied_at)`、sha256 文件字节。
 */
export function applyMigrations(db, repoRoot, { now = Date.now } = {}) {
  const files = listMigrationFiles(repoRoot);
  const table = db
    .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'")
    .get();
  const recorded = new Map();
  if (table) {
    for (const row of db.prepare("SELECT version, checksum FROM schema_migrations").all()) {
      recorded.set(Number(row.version), String(row.checksum));
    }
  }
  const applied = [];
  for (const file of files) {
    const existing = recorded.get(file.version);
    if (existing !== undefined) {
      if (existing !== file.checksum) {
        throw new Error(
          `迁移 checksum 不一致：${file.name}（记录 ${existing} / 重算 ${file.checksum}）`,
        );
      }
      continue;
    }
    db.exec("BEGIN");
    try {
      db.exec(file.sql);
      db.prepare("INSERT INTO schema_migrations(version, checksum, applied_at) VALUES (?, ?, ?)").run(
        file.version,
        file.checksum,
        now(),
      );
      db.exec("COMMIT");
    } catch (error) {
      try {
        db.exec("ROLLBACK");
      } catch {
        // 事务已因错误回滚。
      }
      throw new Error(`应用迁移 ${file.name} 失败：${error.message}`);
    }
    applied.push(file.version);
  }
  return applied;
}

/** 打开连接并执行 D3 PRAGMA 全集（写连接）/只读替代（读连接）。 */
export function openDatabase(dbPath, { readOnly = false, cacheKib = 32_000 } = {}) {
  // node:sqlite 的 readOnly 在 WAL 模式需要可写 -shm；基准读连接用 query_only
  // 复刻 aether-store 读池语义，避免因只读句柄无法附加 WAL 而误判。
  const db = new (requireSqlite().DatabaseSync)(dbPath, { timeout: 5_000 });
  db.exec("PRAGMA busy_timeout = 5000");
  db.exec("PRAGMA foreign_keys = ON");
  db.exec("PRAGMA temp_store = MEMORY");
  db.exec(`PRAGMA cache_size = -${cacheKib}`);
  db.exec("PRAGMA journal_size_limit = 67108864");
  if (readOnly) {
    db.exec("PRAGMA query_only = ON");
  } else {
    db.exec("PRAGMA journal_mode = WAL");
    db.exec("PRAGMA synchronous = NORMAL");
    db.exec("PRAGMA wal_autocheckpoint = 1000");
  }
  return db;
}

export function sqliteVersion(db) {
  return String(db.prepare("SELECT sqlite_version() AS version").get().version);
}

export function fileBytes(filePath) {
  try {
    return statSync(filePath).size;
  } catch {
    return 0;
  }
}

export function sizeSnapshot(dbPath) {
  const db = fileBytes(dbPath);
  const wal = fileBytes(`${dbPath}-wal`);
  const shm = fileBytes(`${dbPath}-shm`);
  return { db, wal, shm, total: db + wal };
}

export function sha256File(filePath) {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(filePath);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("error", reject);
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

export function percentile(sortedAscending, p) {
  if (sortedAscending.length === 0) return 0;
  const index = (sortedAscending.length - 1) * p;
  const lower = Math.floor(index);
  const upper = Math.ceil(index);
  if (lower === upper) return sortedAscending[lower];
  const weight = index - lower;
  return sortedAscending[lower] * (1 - weight) + sortedAscending[upper] * weight;
}

/** 统计摘要：count/min/p50/p95/p99/max/mean（毫秒或条数，不混用）。 */
export function summarize(values) {
  const finite = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (finite.length === 0) {
    return { count: 0, min: 0, p50: 0, p95: 0, p99: 0, max: 0, mean: 0 };
  }
  const total = finite.reduce((sum, value) => sum + value, 0);
  return {
    count: finite.length,
    min: round(finite[0]),
    p50: round(percentile(finite, 0.5)),
    p95: round(percentile(finite, 0.95)),
    p99: round(percentile(finite, 0.99)),
    max: round(finite[finite.length - 1]),
    mean: round(total / finite.length),
  };
}

export function round(value, digits = 3) {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

export function formatBytes(bytes) {
  if (bytes >= GIB) return `${(bytes / GIB).toFixed(2)} GiB`;
  if (bytes >= MIB) return `${(bytes / MIB).toFixed(2)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(2)} KiB`;
  return `${bytes} B`;
}

/** 机器信息：不含用户名/主机名/绝对路径（ADR-038 D-038-5）。 */
export function machineInfo(volumePath) {
  const cpus = os.cpus();
  const info = {
    os: `${os.platform()} ${os.release()}`,
    platform: os.platform(),
    arch: os.arch(),
    cpu: cpus[0]?.model?.trim() ?? "unknown",
    cores: os.cpus().length,
    ramBytes: os.totalmem(),
    node: process.version,
    volume: path.parse(path.resolve(volumePath)).root,
    fileSystem: null,
    diskModel: null,
    diskBusType: null,
    freeBytes: null,
  };
  try {
    const stats = statfsSync(volumePath);
    info.freeBytes = Number(stats.bavail) * Number(stats.bsize);
  } catch {
    // 保留 null；不阻断压测。
  }
  if (process.platform === "win32") {
    const drive = info.volume.replace(/[\\/:]/g, "");
    try {
      const script =
        `$p = Get-Partition -DriveLetter ${drive} -ErrorAction Stop; ` +
        `$d = $p | Get-Disk; ` +
        `[pscustomobject]@{ fileSystem = (Get-Volume -DriveLetter ${drive}).FileSystem; ` +
        `diskModel = $d.FriendlyName; diskBusType = $d.BusType } | ConvertTo-Json -Compress`;
      const raw = execFileSync("powershell", ["-NoProfile", "-NonInteractive", "-Command", script], {
        encoding: "utf8",
        timeout: 15_000,
      }).trim();
      if (raw) Object.assign(info, JSON.parse(raw));
    } catch {
      // 权限/环境不支持时保留 null。
    }
  }
  return info;
}

export function readJson(filePath) {
  return JSON.parse(readFileSync(filePath, "utf8"));
}

export function writeJson(filePath, value) {
  mkdirSync(path.dirname(filePath), { recursive: true });
  writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

export function ensureDir(dir) {
  mkdirSync(dir, { recursive: true });
  return dir;
}

export function repoRootFrom(importMetaUrl) {
  return path.resolve(path.dirname(fileURLToPath(importMetaUrl)), "..", "..", "..");
}

export function exists(filePath) {
  return existsSync(filePath);
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
