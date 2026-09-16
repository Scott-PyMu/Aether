import { describe, expect, it } from "vitest";

import { MockAdapter, type MockAdapterOptions } from "./mock-adapter";

interface Frame {
  jsonrpc?: string;
  id?: number | string;
  method?: string;
  params?: Record<string, unknown>;
  result?: Record<string, unknown>;
  error?: { code: number; message: string };
}

class Harness {
  readonly frames: Frame[] = [];
  readonly raw = { text: "" };
  readonly stderr: string[] = [];
  readonly exits: number[] = [];
  readonly mock: MockAdapter;

  private readonly pending: string[] = [];
  private wake?: () => void;
  private readonly lines = this.lineStream();

  constructor(options: Partial<MockAdapterOptions> = {}) {
    this.mock = new MockAdapter({
      lines: this.lines,
      writeLine: async (line) => {
        this.frames.push(JSON.parse(line) as Frame);
      },
      rawWrite: (chunk) => {
        this.raw.text += chunk;
      },
      stderr: (line) => {
        this.stderr.push(line);
      },
      exit: (code) => {
        this.exits.push(code);
      },
      ...options,
    });
  }

  private async *lineStream(): AsyncGenerator<string> {
    for (;;) {
      if (this.pending.length === 0) {
        await new Promise<void>((resolve) => {
          this.wake = resolve;
        });
      }
      while (this.pending.length > 0) {
        yield this.pending.shift()!;
      }
    }
  }

  async start(): Promise<void> {
    void this.mock.run();
    await this.waitForFrame((frame) => frame.method === "hello", "hello");
  }

  send(line: string): void {
    this.pending.push(line);
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
  }

  request(method: string, params: unknown = {}, id?: number): void {
    const nextId = id ?? this.frames.filter((frame) => frame.id !== undefined).length + 100;
    this.send(JSON.stringify({ jsonrpc: "2.0", id: nextId, method, params }));
  }

  async requestResult(method: string, params: unknown = {}): Promise<Record<string, unknown>> {
    const id = 100_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    const frame = await this.waitForFrame((candidate) => candidate.id === id, `response ${method}`);
    if (frame.error) throw new Error(`RPC error ${frame.error.code}: ${frame.error.message}`);
    return frame.result ?? {};
  }

  requestError(method: string, params: unknown = {}): Promise<Frame> {
    const id = 200_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    return this.waitForFrame((candidate) => candidate.id === id && candidate.error !== undefined, `error ${method}`);
  }

  events(): Frame[] {
    return this.frames.filter((frame) => frame.method === "event");
  }

  async createSession(): Promise<string> {
    const result = await this.requestResult("session.create", { title: "test" });
    return result.session_id as string;
  }

  async sendMessage(
    sessionId: string,
    text: string,
    clientMsgId = `c-${Math.random().toString(36).slice(2)}`,
  ): Promise<string> {
    const result = await this.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: clientMsgId,
      text,
    });
    return result.run_id as string;
  }

  async waitForFrame(predicate: (frame: Frame) => boolean, label = "frame"): Promise<Frame> {
    const deadline = Date.now() + 5000;
    for (;;) {
      const found = this.frames.find(predicate);
      if (found) return found;
      if (Date.now() > deadline) {
        throw new Error(`等待 ${label} 超时；已收帧=${JSON.stringify(this.frames, null, 2)}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  async waitForEvent(type: string): Promise<Frame> {
    return this.waitForFrame(
      (frame) => frame.method === "event" && frame.params?.type === type,
      `event ${type}`,
    );
  }
}

function eventTypesBetween(harness: Harness, runId: string): string[] {
  return harness
    .events()
    .filter((frame) => frame.params?.run_id === runId)
    .map((frame) => frame.params?.type as string);
}

function toolEventTypes(types: string[]): string[] {
  return types.filter((type) => type.startsWith("tool.") || type.startsWith("permission."));
}

describe("Mock 适配器：基础协议路径", () => {
  it("hello + initialize + tools.list", async () => {
    const harness = new Harness();
    await harness.start();
    const hello = harness.frames.find((frame) => frame.method === "hello");
    expect(hello?.params?.protocol).toBe("1.0");

    const initialized = await harness.requestResult("initialize", {});
    expect(initialized.acknowledged).toBe(true);
    expect((initialized.capabilities as string[]).length).toBeGreaterThan(0);

    const tools = await harness.requestResult("tools.list", {});
    expect((tools.tools as unknown[]).length).toBeGreaterThan(0);
  });

  it("未知方法回 -32601 且连接可用（D6 前向兼容）", async () => {
    const harness = new Harness();
    await harness.start();
    const error = await harness.requestError("session.listen", {});
    expect(error.error?.code).toBe(-32601);
    const pong = await harness.requestResult("health.ping", {});
    expect(pong.status).toBe("ok");
  });

  it("未知会话 → 1005；capability 缺失 → 1004", async () => {
    const harness = new Harness({ injections: ["capability-missing"] });
    await harness.start();
    const missing = await harness.requestError("session.send", {
      session_id: "nope",
      client_msg_id: "c1",
      text: "hi",
    });
    expect(missing.error?.code).toBe(1005);
    const capability = await harness.requestError("tools.list", {});
    expect(capability.error?.code).toBe(1004);
  });

  it("session.send 幂等（client_msg_id 重复不产生新 run）", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const first = await harness.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: "dup-1",
      text: "hello",
    });
    const second = await harness.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: "dup-1",
      text: "hello",
    });
    expect(second.run_id).toBe(first.run_id);
    expect(second.duplicate).toBe(true);
  });

  it("流式跑完：run.started → message.delta×N → message.completed → run.completed", async () => {
    const harness = new Harness({ streamDeltas: 24, streamIntervalMs: 1 });
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "普通消息");
    await harness.waitForEvent("run.completed");
    const types = eventTypesBetween(harness, runId);
    expect(types.filter((type) => type === "message.delta")).toHaveLength(24);
    expect(types[0]).toBe("run.started");
    expect(types[types.length - 1]).toBe("run.completed");

    const completed = harness.events().find((frame) => frame.params?.type === "message.completed");
    const message = completed?.params?.payload as { content: string; message: { content: string } };
    const deltas = harness
      .events()
      .filter((frame) => frame.params?.type === "message.delta")
      .map((frame) => (frame.params?.payload as { text: string }).text)
      .join("");
    expect(message.message.content).toBe(deltas);

    const seqs = harness
      .events()
      .filter((frame) => frame.params?.session_id === sessionId)
      .map((frame) => frame.params?.seq as number);
    expect(seqs).toEqual([...seqs].sort((a, b) => a - b));
    expect(new Set(seqs).size).toBe(seqs.length);
  });
});

describe("Mock 适配器：5 类工具调用注入清单（DoD6 权威序列）", () => {
  it("① 正常完成：tool.call_started → tool.call_completed", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "tool:normal");
    await harness.waitForEvent("run.completed");
    expect(toolEventTypes(eventTypesBetween(harness, runId))).toEqual([
      "tool.call_started",
      "tool.call_completed",
    ]);
  });

  it("② 执行失败：tool.call_started → tool.call_failed(error)", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "tool:fail");
    await harness.waitForEvent("run.completed");
    const types = toolEventTypes(eventTypesBetween(harness, runId));
    expect(types).toEqual(["tool.call_started", "tool.call_failed"]);
    const failed = harness.events().find((frame) => frame.params?.type === "tool.call_failed");
    const error = (failed?.params?.payload as { error: { code: string } }).error;
    expect(error.code).toBe("tool_execution_failed");
  });

  it("③ 超时中断：tool.call_started → tool.call_failed(timeout/abort) + run.cancelled", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "tool:timeout");
    await harness.waitForEvent("tool.call_started");
    const interrupted = await harness.requestResult("session.interrupt", { session_id: sessionId });
    expect(interrupted.interrupted).toBe(true);
    await harness.waitForEvent("run.cancelled");
    const types = toolEventTypes(eventTypesBetween(harness, runId));
    expect(types).toEqual(["tool.call_started", "tool.call_failed"]);
    const failed = harness.events().find((frame) => frame.params?.type === "tool.call_failed");
    const error = (failed?.params?.payload as { error: { code: string } }).error;
    expect(error.code).toBe("timeout");
  });

  it("④ 权限 ask→允许：permission.requested → permission.resolved(allow) + tool.call_completed", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "tool:permission-allow");
    const requested = await harness.waitForEvent("permission.requested");
    expect(harness.frames.some((frame) => frame.method === "permission.request")).toBe(true);
    const requestId = (requested.params?.payload as { request_id: string }).request_id;
    await harness.requestResult("permission.resolve", {
      request_id: requestId,
      decision: "allow",
      scope: "once",
    });
    await harness.waitForEvent("run.completed");
    expect(toolEventTypes(eventTypesBetween(harness, runId))).toEqual([
      "permission.requested",
      "permission.resolved",
      "tool.call_completed",
    ]);
    const resolved = harness.events().find((frame) => frame.params?.type === "permission.resolved");
    expect((resolved?.params?.payload as { decision: string }).decision).toBe("allow");
  });

  it("⑤ 权限 ask→拒绝：permission.requested → permission.resolved(deny) + tool.call_failed(denied)", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "tool:permission-deny");
    const requested = await harness.waitForEvent("permission.requested");
    const requestId = (requested.params?.payload as { request_id: string }).request_id;
    await harness.requestResult("permission.resolve", {
      request_id: requestId,
      decision: "deny",
      scope: "once",
    });
    await harness.waitForEvent("run.completed");
    expect(toolEventTypes(eventTypesBetween(harness, runId))).toEqual([
      "permission.requested",
      "permission.resolved",
      "tool.call_failed",
    ]);
    const failed = harness.events().find((frame) => frame.params?.type === "tool.call_failed");
    expect((failed?.params?.payload as { error: { code: string } }).error.code).toBe("denied");
  });
});

describe("Mock 适配器：生命周期与基准", () => {
  it("session.dispose 后会话不可用（1005）", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const disposed = await harness.requestResult("session.dispose", { session_id: sessionId });
    expect(disposed.disposed).toBe(true);
    const error = await harness.requestError("session.send", {
      session_id: sessionId,
      client_msg_id: "c-after",
      text: "hi",
    });
    expect(error.error?.code).toBe(1005);
  });

  it("shutdown 回复 ok 并退出码 0", async () => {
    const harness = new Harness();
    await harness.start();
    const result = await harness.requestResult("shutdown", {});
    expect(result.ok).toBe(true);
    expect(harness.exits).toContain(0);
  });

  it("bench:N 连发 N 条 delta 且不丢帧", async () => {
    const harness = new Harness();
    await harness.start();
    const sessionId = await harness.createSession();
    const runId = await harness.sendMessage(sessionId, "bench:500");
    await harness.waitForEvent("run.completed");
    const deltas = harness
      .events()
      .filter((frame) => frame.params?.run_id === runId && frame.params?.type === "message.delta");
    expect(deltas).toHaveLength(500);
  });
});

describe("Mock 适配器：故障注入", () => {
  it("bad-json：hello 后连发 20 条坏 JSON", async () => {
    const harness = new Harness({ injections: ["bad-json"], injectCount: 20 });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.split("\n").filter(Boolean).length >= 20, "20 条坏 JSON");
    const badLines = harness.raw.text.split("\n").filter(Boolean);
    expect(badLines).toHaveLength(20);
    for (const line of badLines) {
      expect(() => JSON.parse(line)).toThrow();
    }
  });

  it("stdout-log：hello 后混入 N 条日志", async () => {
    const harness = new Harness({ injections: ["stdout-log"], injectCount: 5 });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.split("\n").filter(Boolean).length >= 5, "5 条日志");
    expect(harness.raw.text).toContain("[mock]");
  });

  it("half-line：写残行后退出", async () => {
    const harness = new Harness({ injections: ["half-line"] });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.length > 0, "残行");
    expect(harness.raw.text.endsWith("params\":")).toBe(true);
    await harness.waitForFrame(() => harness.exits.length > 0, "退出");
    expect(harness.exits[0]).toBe(0);
  });

  it("oversized-line：1–2MiB 非 artifact_ref 行（D6 正常解析样例）", async () => {
    const harness = new Harness({ injections: ["oversized-line"] });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.includes('"pad"'), "超大行");
    expect(harness.raw.text.length).toBeGreaterThan(1024 * 1024);
    expect(harness.raw.text.length).toBeLessThan(2 * 1024 * 1024);
    expect(harness.raw.text.includes('"type":"artifact_ref"')).toBe(false);
  });

  it("line-over-2mib：>2MiB 任意行（D6 断连样例）", async () => {
    const harness = new Harness({ injections: ["line-over-2mib"] });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.includes('"pad"'), "超 2MiB 行");
    expect(harness.raw.text.length).toBeGreaterThan(2 * 1024 * 1024);
  });

  it("artifact-line：<1MiB artifact_ref 行（顶层 type，D6 契约内样例）", async () => {
    const harness = new Harness({ injections: ["artifact-line"] });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.includes('"type":"artifact_ref"'), "引用行");
    expect(harness.raw.text.length).toBeGreaterThan(256 * 1024);
    expect(harness.raw.text.length).toBeLessThan(1024 * 1024);
  });

  it("artifact-line-over-limit：1–2MiB 声称 artifact_ref（D6 契约违约样例）", async () => {
    const harness = new Harness({ injections: ["artifact-line-over-limit"] });
    await harness.start();
    await harness.waitForFrame(() => harness.raw.text.includes('"type":"artifact_ref"'), "违约引用行");
    expect(harness.raw.text.length).toBeGreaterThan(1024 * 1024);
    expect(harness.raw.text.length).toBeLessThan(2 * 1024 * 1024);
  });

  it("crash：退出码 41", async () => {
    const harness = new Harness({ injections: ["crash"] });
    await harness.start();
    await harness.waitForFrame(() => harness.exits.length > 0, "退出");
    expect(harness.exits[0]).toBe(41);
  });

  it("no-hello：不发 hello", async () => {
    const harness = new Harness({ injections: ["no-hello"] });
    void harness.mock.run();
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(harness.frames).toHaveLength(0);
  });

  it("hang：不响应 health/请求且不退出（DoD7 不响应模式）", async () => {
    const harness = new Harness({ injections: ["hang"] });
    await harness.start();
    harness.request("health.ping", {});
    harness.request("session.create", { title: "x" });
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(harness.frames.filter((frame) => frame.id !== undefined)).toHaveLength(0);
    expect(harness.exits).toHaveLength(0);
  });

  it("协议版本覆盖：hello 携带指定 protocol", async () => {
    const harness = new Harness({ protocol: "2.0" });
    await harness.start();
    const hello = harness.frames.find((frame) => frame.method === "hello");
    expect(hello?.params?.protocol).toBe("2.0");
  });
});
