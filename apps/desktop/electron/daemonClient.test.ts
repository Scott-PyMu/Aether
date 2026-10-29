/**
 * `DaemonClient` 单测（M9-07 DoD-1/DoD-4；ADR-029 §4.1/§4.3）。
 *
 * 用 PassThrough 假子进程验证：JSONL 分帧（跨 chunk）、id 匹配、通知、
 * 首帧致命错误、2MiB 上限、stderr 环形缓冲、超时与退出拒绝。
 */
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import type { ChildProcessWithoutNullStreams } from "node:child_process";

import { describe, expect, it } from "vitest";

import {
  AetherRpcError,
  AetherRpcTimeoutError,
  DaemonClient,
  MAX_FRAME_BYTES,
  type DaemonNotification,
  type SpawnFunction,
} from "./daemonClient";

class FakeChild extends EventEmitter {
  readonly stdin = new PassThrough();
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  readonly pid = 4_242;
  killed = false;
  readonly written: string[] = [];

  constructor() {
    super();
    this.stdin.on("data", (chunk: Buffer) => {
      this.written.push(chunk.toString("utf8"));
    });
  }

  kill(): boolean {
    this.killed = true;
    this.emit("exit", null, "SIGTERM");
    return true;
  }

  push(frame: unknown): void {
    this.stdout.write(`${JSON.stringify(frame)}\n`);
  }

  pushRaw(line: string): void {
    this.stdout.write(line);
  }

  exit(code: number | null): void {
    this.emit("exit", code, null);
  }
}

interface Harness {
  client: DaemonClient;
  child: FakeChild;
  notifications: DaemonNotification[];
  spawnCount: number;
}

function harness(overrides: { maxFrameBytes?: number } = {}): Harness {
  const child = new FakeChild();
  let spawnCount = 0;
  const spawnFn: SpawnFunction = () => {
    spawnCount += 1;
    return child as unknown as ChildProcessWithoutNullStreams;
  };
  const notifications: DaemonNotification[] = [];
  const client = new DaemonClient({
    spawn: spawnFn,
    spawnSpec: { command: "aetherd", args: ["--stdio"], env: {} },
    onNotification: (notification) => notifications.push(notification),
    maxFrameBytes: overrides.maxFrameBytes,
  });
  return {
    client,
    child,
    notifications,
    get spawnCount() {
      return spawnCount;
    },
  };
}

function requestOf(child: FakeChild, index = 0): Record<string, unknown> {
  const line = child.written[index];
  if (line === undefined) {
    throw new Error(`未写出第 ${index} 个请求`);
  }
  return JSON.parse(line.trim()) as Record<string, unknown>;
}

const INITIALIZE_PARAMS = {
  protocolVersion: "1.0",
  appVersion: "0.1.0",
  client: "electron",
  clientCapabilities: ["events"],
  requiredCapabilities: ["events"],
};

describe("DaemonClient", () => {
  it("JSONL 分帧：跨 chunk 的 initialize 响应可解析", async () => {
    const { client, child } = harness();
    client.start();
    const promise = client.initialize(INITIALIZE_PARAMS, 1_000);
    const request = requestOf(child);
    expect(request.method).toBe("initialize");
    const response = JSON.stringify({
      jsonrpc: "2.0",
      id: request.id,
      result: {
        protocolVersion: "1.0",
        daemonVersion: "0.1.0",
        coreVersion: "0.1.0",
        dataDir: "C:\\Aether",
        phase: "warming",
        daemonCapabilities: ["events"],
      },
    });
    child.pushRaw(response.slice(0, 20));
    child.pushRaw(`${response.slice(20)}\n`);
    await expect(promise).resolves.toMatchObject({ phase: "warming" });
    expect(child.written[0]).toContain('"jsonrpc":"2.0"');
  });

  it("通知按到达顺序分发（aether.event / daemon.status）", () => {
    const { client, child, notifications } = harness();
    client.start();
    child.push({
      jsonrpc: "2.0",
      method: "aether.event",
      params: { v: 1, seq: 1 },
    });
    child.push({
      jsonrpc: "2.0",
      method: "daemon.status",
      params: { phase: "ready" },
    });
    expect(notifications.map((item) => item.method)).toEqual([
      "aether.event",
      "daemon.status",
    ]);
  });

  it("首帧 id:null error：initialize 以 business_code 拒绝并记录 bootstrapFailure", async () => {
    const { client, child } = harness();
    client.start();
    const promise = client.initialize(INITIALIZE_PARAMS, 1_000);
    child.push({
      jsonrpc: "2.0",
      id: null,
      error: {
        code: -32005,
        message: "A4 阻断",
        data: {
          aether_code: "startup_blocked",
          business_code: "sync_dir_blocked",
          phase: "failed",
          log_path: "C:\\Aether\\logs\\aetherd.log",
        },
      },
    });
    await expect(promise).rejects.toMatchObject({
      code: -32005,
      aetherCode: "startup_blocked",
      businessCode: "sync_dir_blocked",
    });
    expect(client.bootstrapFailure?.businessCode).toBe("sync_dir_blocked");
    client.kill();
    expect(client.isClosed).toBe(true);
  });

  it("超过 2MiB 的输出帧：拒绝在途请求并关闭连接", async () => {
    const { client, child } = harness();
    client.start();
    const promise = client.initialize(INITIALIZE_PARAMS, 1_000);
    child.pushRaw("x".repeat(MAX_FRAME_BYTES + 1));
    child.pushRaw("\n");
    await expect(promise).rejects.toBeInstanceOf(AetherRpcError);
    expect(client.isClosed).toBe(true);
    expect(child.killed).toBe(true);
  });

  it("stderr 环形缓冲只保留尾部 200 行", () => {
    const { client, child } = harness();
    client.start();
    for (let index = 0; index < 260; index += 1) {
      child.stderr.write(`line-${index}\n`);
    }
    const tail = client.stderrTail.split("\n");
    expect(tail).toHaveLength(200);
    expect(tail[0]).toBe("line-60");
    expect(tail[199]).toBe("line-259");
  });

  it("请求超时抛出 AetherRpcTimeoutError（不静默挂起）", async () => {
    const { client } = harness();
    client.start();
    await expect(client.initialize(INITIALIZE_PARAMS, 20)).rejects.toBeInstanceOf(
      AetherRpcTimeoutError,
    );
  });

  it("进程退出：在途请求以 core_not_ready 拒绝", async () => {
    const { client, child } = harness();
    client.start();
    const promise = client.initialize(INITIALIZE_PARAMS, 1_000);
    child.exit(0);
    await expect(promise).rejects.toMatchObject({ aetherCode: "core_not_ready" });
    expect(client.exitInfo).toEqual({ code: 0, signal: null });
  });
});
