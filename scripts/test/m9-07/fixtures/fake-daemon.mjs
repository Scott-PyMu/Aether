#!/usr/bin/env node
/**
 * M9-07 E2E 假 daemon（仅测试；由 `AETHERD_BIN=node` +
 * `AETHERD_EXTRA_ARGS=["<本文件>"]` 注入）。
 *
 * 目的：在不依赖真实数据目录/同步盘/损坏 DB 的前提下，驱动 Electron main 的
 * 启动分支（首帧致命、握手超时重试、degraded 安全模式）。真实 daemon 的
 * `db_open_failed` / `data_dir_locked` 由 E2E 另外用 `target/debug/aetherd`
 * 覆盖；假 daemon 只替代无法稳定构造的分支。
 *
 * 模式（`AETHER_FAKE_MODE`）：
 * - healthy：initialize 成功；daemon.status=ready；health=normal
 * - delayed：1.2s 后 initialize（用于断言 S-00 先出现）
 * - degraded：initialize 成功；daemon.status=degraded；health.persist_degraded
 * - timeout_once：首次静默（触发 10s 硬超时），第二次 healthy
 * - bootstrap_once：首次 db_open_failed 首帧错误，第二次 healthy
 * - bootstrap:<business_code>：立即首帧错误并退出
 * - exit：立即退出（无首帧）
 */
import { existsSync, writeFileSync } from "node:fs";
import process from "node:process";
import readline from "node:readline";

const mode = process.env.AETHER_FAKE_MODE ?? "healthy";
const stateFile = process.env.AETHER_FAKE_STATE ?? "";
const logPath =
  process.env.AETHER_FAKE_LOG_PATH ??
  (process.env.AETHER_DATA_DIR
    ? `${process.env.AETHER_DATA_DIR}/logs/aetherd.log`
    : "aetherd.log");

const BOOTSTRAP = {
  sync_dir_blocked: { aether_code: "startup_blocked", exit: 21 },
  data_dir_locked: { aether_code: "internal", exit: 20 },
  db_open_failed: { aether_code: "internal", exit: 22 },
  schema_checksum_mismatch: { aether_code: "internal", exit: 22 },
};

function send(frame) {
  process.stdout.write(`${JSON.stringify(frame)}\n`);
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

function result(id, value) {
  send({ jsonrpc: "2.0", id, result: value });
}

function bootstrapError(businessCode) {
  const info = BOOTSTRAP[businessCode] ?? { aether_code: "internal", exit: 24 };
  send({
    jsonrpc: "2.0",
    id: null,
    error: {
      code: -32005,
      message: `E2E 注入启动失败：${businessCode}`,
      data: {
        aether_code: info.aether_code,
        business_code: businessCode,
        phase: "failed",
        log_path: logPath,
      },
    },
  });
  process.exit(info.exit);
}

function onceState() {
  if (!stateFile) {
    return false;
  }
  if (existsSync(stateFile)) {
    return false;
  }
  writeFileSync(stateFile, "first", "utf8");
  return true;
}

if (mode === "exit") {
  process.exit(1);
}

if (mode.startsWith("bootstrap:")) {
  bootstrapError(mode.slice("bootstrap:".length));
}

if (mode === "bootstrap_once" && onceState()) {
  bootstrapError("db_open_failed");
}

if (mode === "timeout_once" && onceState()) {
  // 保持进程存活但不响应 initialize（main 10s 硬超时后 kill 并重试）。
  setInterval(() => {}, 1_000);
} else {
  const rl = readline.createInterface({ input: process.stdin });
  rl.on("line", (line) => {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      return;
    }
    if (frame.method === "initialize") {
      if (mode === "handshake") {
        send({
          jsonrpc: "2.0",
          id: frame.id,
          error: {
            code: -32001,
            message: "E2E 注入协议版本不一致",
            data: { reason: "protocol_mismatch" },
          },
        });
        return;
      }
      const respond = () => {
        result(frame.id, {
          protocolVersion: "1.0",
          daemonVersion: "0.1.0-e2e",
          coreVersion: "0.1.0-e2e",
          dataDir: process.env.AETHER_DATA_DIR ?? "C:\\Aether",
          phase: "warming",
          daemonCapabilities: ["events", "permissions", "dialogs"],
        });
        setTimeout(() => {
          notify("daemon.status", {
            phase: mode === "degraded" ? "degraded" : "ready",
            since_ms: Date.now(),
          });
        }, 120);
      };
      if (mode === "delayed") {
        setTimeout(respond, 1_200);
      } else {
        respond();
      }
      return;
    }
    if (frame.method === "health") {
      result(frame.id, {
        storage_state: mode === "degraded" ? "persist_degraded" : "normal",
        runtimes: [],
        pending_permissions: 0,
      });
      return;
    }
    if (frame.method === "session_list") {
      result(frame.id, { sessions: [] });
      return;
    }
    if (frame.method === "shutdown") {
      result(frame.id, { accepted: true, shutdown_id: "e2e-shutdown" });
      setTimeout(() => process.exit(0), 50);
      return;
    }
    // 其他方法：返回空对象，避免 E2E 卡住。
    result(frame.id, {});
  });
  rl.on("close", () => {
    process.exit(0);
  });
}
