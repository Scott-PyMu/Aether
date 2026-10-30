/**
 * 极简 daemon JSONL RPC 客户端（M9-11 安装冒烟）。
 *
 * 只用 Node 标准库；与 `aether-app`/`aether-rpc` 的线协议口径一致：
 * JSON-RPC 2.0 over JSON-Lines（单帧 ≤2MiB；本脚本请求均远小于上限）。
 * 不替代 M9-03 的协议测试，只用于驱动**已安装产物**内的 `aetherd`。
 */
import { spawn } from "node:child_process";

const MAX_FRAME_BYTES = 2 * 1024 * 1024;

export class DaemonRpc {
  constructor({ binary, args = ["--stdio"], env = {}, cwd }) {
    this.binary = binary;
    this.child = spawn(binary, args, {
      cwd,
      env: { ...process.env, ...env },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    this.nextId = 1;
    this.pending = new Map();
    this.notifications = [];
    this.waiters = new Set();
    this.stdout = "";
    this.stderr = "";
    this.exited = null;
    this.exitWaiters = [];

    this.child.stdout.on("data", (chunk) => this.#onData(chunk));
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
      if (this.stderr.length > 256 * 1024) this.stderr = this.stderr.slice(-256 * 1024);
    });
    this.child.on("exit", (code, signal) => {
      this.exited = { code, signal };
      for (const [, entry] of this.pending) {
        entry.reject(new Error(`daemon 已退出（code=${code} signal=${signal}）`));
      }
      this.pending.clear();
      for (const waiter of this.exitWaiters.splice(0)) waiter(this.exited);
    });
    this.child.on("error", (error) => {
      for (const [, entry] of this.pending) entry.reject(error);
      this.pending.clear();
    });
  }

  #onData(chunk) {
    this.stdout += chunk.toString("utf8");
    for (;;) {
      const newline = this.stdout.indexOf("\n");
      if (newline < 0) break;
      const line = this.stdout.slice(0, newline);
      this.stdout = this.stdout.slice(newline + 1);
      if (line.trim().length === 0) continue;
      if (Buffer.byteLength(line) > MAX_FRAME_BYTES) {
        this.notifications.push({ method: "<oversize>", params: { bytes: Buffer.byteLength(line) } });
        continue;
      }
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        this.notifications.push({ method: "<invalid-json>", params: { line } });
        continue;
      }
      if (frame && typeof frame === "object" && frame.id !== undefined && frame.id !== null) {
        const entry = this.pending.get(frame.id);
        if (entry) {
          this.pending.delete(frame.id);
          if (frame.error) entry.reject(new RpcError(frame.error));
          else entry.resolve(frame.result);
        }
        continue;
      }
      const notification = { method: frame?.method ?? "<unknown>", params: frame?.params };
      this.notifications.push(notification);
      for (const waiter of [...this.waiters]) {
        if (waiter.predicate(notification)) {
          this.waiters.delete(waiter);
          waiter.resolve(notification);
        }
      }
    }
  }

  request(method, params, timeoutMs = 30_000) {
    const id = this.nextId++;
    const payload = { jsonrpc: "2.0", id, method, params: params ?? {} };
    const line = `${JSON.stringify(payload)}\n`;
    if (Buffer.byteLength(line) > MAX_FRAME_BYTES) {
      return Promise.reject(new Error(`请求帧超过 2MiB：${method}`));
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`daemon 请求超时（${timeoutMs}ms）：${method}`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.child.stdin.write(line);
    });
  }

  waitForNotification(predicate, timeoutMs, label = "通知") {
    const existing = this.notifications.find(predicate);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const waiter = {
        predicate,
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
      };
      const timer = setTimeout(() => {
        this.waiters.delete(waiter);
        reject(new Error(`等待超时（${timeoutMs}ms）：${label}`));
      }, timeoutMs);
      this.waiters.add(waiter);
    });
  }

  exit(timeoutMs = 10_000) {
    if (this.exited) return Promise.resolve(this.exited);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`等待 daemon 退出超时（${timeoutMs}ms）`));
      }, timeoutMs);
      this.exitWaiters.push((value) => {
        clearTimeout(timer);
        resolve(value);
      });
    });
  }

  async shutdown(timeoutMs = 10_000) {
    try {
      await this.request("shutdown", {}, 5_000);
    } catch {
      // shutdown 回执可能先于进程退出；连接已断按已关闭处理。
    }
    this.child.stdin.end();
    return this.exit(timeoutMs);
  }

  kill() {
    if (this.child.exitCode !== null) return;
    try {
      this.child.kill("SIGKILL");
    } catch {
      // 已退出。
    }
  }
}

export class RpcError extends Error {
  constructor(error) {
    super(`${error?.message ?? "rpc error"}（code=${error?.code ?? "?"}）`);
    this.code = error?.code;
    this.data = error?.data;
  }
}

/** 事件判定：`aether.event` 且 payload.type 命中。 */
export function isEvent(type) {
  return (notification) =>
    notification.method === "aether.event" && notification.params?.type === type;
}
