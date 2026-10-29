#!/usr/bin/env node
/**
 * M9-09 故障注入夹具：daemon 收到 `shutdown` 后仍不退出（模拟卡死），并派生一个
 * 子进程用于断言「整树回收、无孤儿」。
 *
 * 由 `AETHERD_BIN=node` + `AETHERD_EXTRA_ARGS=["<本文件>"]` 注入；仅测试使用。
 * 环境变量：
 * - `AETHER_STUCK_CHILD_PID_FILE`：子进程 pid 写入路径；
 * - `AETHER_STUCK_MODE=ignore-shutdown`（默认）。
 */
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import process from "node:process";
import readline from "node:readline";

const mode = process.env.AETHER_STUCK_MODE ?? "ignore-shutdown";
const pidFile = process.env.AETHER_STUCK_CHILD_PID_FILE ?? "";
const logPath =
  process.env.AETHER_FAKE_LOG_PATH ??
  (process.env.AETHER_DATA_DIR
    ? `${process.env.AETHER_DATA_DIR}/logs/aetherd.log`
    : "aetherd.log");

function send(frame) {
  process.stdout.write(`${JSON.stringify(frame)}\n`);
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

function result(id, value) {
  send({ jsonrpc: "2.0", id, result: value });
}

// 子进程继承 daemon 的进程组（Unix）/进程树（Windows），供整树回收断言。
const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
  stdio: "ignore",
  windowsHide: true,
});
if (pidFile) {
  writeFileSync(pidFile, String(child.pid ?? 0), "utf8");
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let frame;
  try {
    frame = JSON.parse(line);
  } catch {
    return;
  }
  if (frame.method === "initialize") {
    result(frame.id, {
      protocolVersion: "1.0",
      daemonVersion: "0.1.0-m9-09-stuck",
      coreVersion: "0.1.0-m9-09-stuck",
      dataDir: process.env.AETHER_DATA_DIR ?? process.cwd(),
      phase: "warming",
      daemonCapabilities: ["events", "permissions", "dialogs"],
    });
    setTimeout(() => {
      notify("daemon.status", { phase: "ready", since_ms: Date.now() });
    }, 50);
    return;
  }
  if (frame.method === "health") {
    result(frame.id, {
      storage_state: "normal",
      write_queue_depth: 0,
      runtimes: [],
      ts: Date.now(),
    });
    return;
  }
  if (frame.method === "session_list") {
    result(frame.id, []);
    return;
  }
  if (frame.method === "runtimes_list") {
    result(frame.id, []);
    return;
  }
  if (frame.method === "permissions_pending") {
    result(frame.id, []);
    return;
  }
  if (frame.method === "workspaces_list") {
    result(frame.id, {
      workspaces: [],
      unassigned: {
        name: "未分配",
        root_path: null,
        active_sessions: 0,
        total_sessions: 0,
      },
    });
    return;
  }
  if (frame.method === "providers_list") {
    result(frame.id, { providers: [] });
    return;
  }
  if (frame.method === "presets_list") {
    result(frame.id, { presets: [] });
    return;
  }
  if (frame.method === "settings_get") {
    result(frame.id, { key: frame.params?.key ?? "", value: null });
    return;
  }
  if (frame.method === "settings_set") {
    result(frame.id, { key: frame.params?.key ?? "", updated: true });
    return;
  }
  if (frame.method === "startup_get") {
    result(frame.id, {
      phase: "ready",
      data_dir: process.env.AETHER_DATA_DIR ?? process.cwd(),
      data_dir_source: "env_override",
      security_level: { level: "os", detail: "e2e stuck" },
    });
    return;
  }
  if (frame.method === "shutdown") {
    // 立即回执但故意不退出；main 应在 8.2s 后整树强杀。
    result(frame.id, { accepted: true, shutdown_id: "stuck-shutdown" });
    if (mode === "ignore-shutdown") {
      setInterval(() => {}, 1_000);
    } else {
      setTimeout(() => process.exit(0), 50);
    }
    return;
  }
  result(frame.id, {});
});
// 故意不处理 stdin close：模拟 daemon 卡死（反向孤儿保护的最坏情况）。
rl.on("close", () => {
  if (mode !== "ignore-shutdown") {
    process.exit(0);
  }
});
