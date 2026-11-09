#!/usr/bin/env node
/**
 * M10-03 DoD-7：独立外部消费者（Node fetch/WebSocket，非 Rust/CLI）。
 *
 * 只依赖 Node 内置 fetch/WebSocket；启动 `aetherd --http-only`，经发现文件读取
 * port/token，验证：
 *  - GET /v1/meta（59 daemon 命令）；
 *  - POST /v1/rpc health（api.state=listening）；
 *  - POST /v1/rpc session_list（空库 → []）；
 *  - WS /v1/events 首帧 auth + subscribe（无 session）。
 *
 * 用法：node scripts/test/m10-03/external-client.mjs [aetherd 路径]
 * 缺省路径：target/debug/aetherd(.exe) 或 AETHERD_BIN。
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const exeName = process.platform === "win32" ? "aetherd.exe" : "aetherd";
const candidates = [
  process.argv[2],
  process.env.AETHERD_BIN,
  path.join(root, "target/debug", exeName),
  path.join(root, "target/debug/deps", exeName),
].filter(Boolean);
const binary = candidates.find((candidate) => existsSync(candidate));
if (!binary) {
  console.error("[external-client] 找不到 aetherd；请先 cargo build -p aether-daemon");
  process.exit(2);
}

const work = mkdtempSync(path.join(tmpdir(), "aether-m10-03-"));
const dataDir = path.join(work, "data");
const registryDir = path.join(work, "registry");
mkdirSync(dataDir, { recursive: true });
mkdirSync(registryDir, { recursive: true });

const child = spawn(binary, ["--http-only"], {
  env: {
    ...process.env,
    AETHER_DATA_DIR: dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(work, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registryDir,
  },
  stdio: ["ignore", "ignore", "pipe"],
  windowsHide: true,
});
let stderr = "";
child.stderr.on("data", (chunk) => {
  stderr += chunk.toString();
});

async function waitDiscovery(timeoutMs = 20000) {
  const file = path.join(dataDir, ".aetherd-api.json");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(file)) {
      try {
        return JSON.parse(readFileSync(file, "utf8"));
      } catch {
        // 写入中，重试
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`等待发现文件超时：${file}`);
}

async function rpc(base, token, method, params) {
  const response = await fetch(`${base}/v1/rpc`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const value = await response.json();
  if (response.status !== 200 || value.error) {
    throw new Error(`${method} 失败：HTTP ${response.status} ${JSON.stringify(value)}`);
  }
  return value.result;
}

async function waitHealth(base, token, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const response = await fetch(`${base}/v1/health`, {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (response.status === 200) return response.json();
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("等待 health=200 超时");
}

function websocketAuth(base, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${base.replace(/^http/, "ws")}/v1/events`);
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error("WS 鉴权超时"));
    }, 10000);
    ws.addEventListener("open", () => {
      ws.send(JSON.stringify({ type: "auth", token }));
      ws.send(JSON.stringify({ type: "subscribe" }));
    });
    ws.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error("WS 连接错误"));
    });
    ws.addEventListener("close", (event) => {
      if (event.code === 4401) {
        clearTimeout(timer);
        reject(new Error("WS 鉴权失败（4401）"));
      }
    });
    ws.addEventListener("message", (event) => {
      const value = JSON.parse(event.data);
      if (value.type === "event" || value.type === "gap") {
        clearTimeout(timer);
        ws.close();
        resolve(value.type);
      }
    });
    // 无会话订阅时不应有事件；仅验证握手不被 4401 拒绝。
    setTimeout(() => {
      clearTimeout(timer);
      ws.close();
      resolve("subscribed");
    }, 800);
  });
}

async function main() {
  const discovery = await waitDiscovery();
  const base = `http://${discovery.bind}:${discovery.port}`;
  const token = discovery.token;

  const meta = await fetch(`${base}/v1/meta`, {
    headers: { Authorization: `Bearer ${token}` },
  }).then((response) => response.json());
  if ((meta.commands ?? []).length < 59) {
    throw new Error(`/v1/meta 命令数 ${meta.commands?.length} < M9 基线 59`);
  }
  if (meta.protocol !== "1.0") throw new Error(`protocol=${meta.protocol} ≠ 1.0`);

  const health = await waitHealth(base, token);
  if (health.api?.state !== "listening") {
    throw new Error(`health.api.state=${health.api?.state} ≠ listening`);
  }
  const sessions = await rpc(base, token, "session_list", {});
  if (!Array.isArray(sessions)) {
    throw new Error(`session_list 返回非数组：${JSON.stringify(sessions)}`);
  }
  const ws = await websocketAuth(base, token);

  console.log(
    JSON.stringify(
      {
        consumer: "node-fetch+websocket",
        protocol: meta.protocol,
        commands: meta.commands.length,
        health: health.api.state,
        sessions: sessions.length,
        ws,
      },
      null,
      2,
    ),
  );
}

function stopChild() {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.killed) {
      resolve();
      return;
    }
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      resolve();
    }, 3000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
    child.kill();
  });
}

function cleanup() {
  try {
    rmSync(work, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  } catch {
    // Windows 上守护进程刚退出时锁文件可能仍被 OS 短暂持有；测试目录可留给系统清理。
  }
}

main()
  .then(async () => {
    await stopChild();
    cleanup();
    process.exit(0);
  })
  .catch(async (error) => {
    console.error(`[external-client] ${error.message}`);
    if (stderr) console.error(stderr);
    await stopChild();
    cleanup();
    process.exit(1);
  });
