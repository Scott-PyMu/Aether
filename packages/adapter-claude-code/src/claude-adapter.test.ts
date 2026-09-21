import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { ClaudeCodeAdapter, type ClaudeAdapterOptions } from "./claude-adapter";
import type { ClaudeCliConfig } from "./claude-cli";

const FAKE_CLI = fileURLToPath(
  new URL("../../../scripts/test/m2-02/fake-claude/cli.mjs", import.meta.url),
);

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
  readonly stderr: string[] = [];
  readonly exits: number[] = [];
  readonly adapter: ClaudeCodeAdapter;
  readonly home: string;
  readonly pidFile: string;

  private readonly pending: string[] = [];
  private wake?: () => void;
  private readonly lines = this.lineStream();

  constructor(options: Partial<ClaudeAdapterOptions> = {}) {
    this.home = mkdtempSync(join(tmpdir(), "fake-claude-"));
    this.pidFile = join(this.home, "pids.txt");
    process.env.FAKE_CLAUDE_HOME = this.home;
    process.env.FAKE_CLAUDE_PID_FILE = this.pidFile;
    const cli: ClaudeCliConfig = {
      bin: process.execPath,
      extraArgs: [FAKE_CLI],
      workspace: process.cwd(),
      tools: "none",
      ...(options.cli ?? {}),
    };
    this.adapter = new ClaudeCodeAdapter({
      lines: this.lines,
      writeLine: async (line) => {
        this.frames.push(JSON.parse(line) as Frame);
      },
      stderr: (line) => {
        this.stderr.push(line);
      },
      exit: (code) => {
        this.exits.push(code);
      },
      cli,
      ...(options.runTimeoutMs !== undefined ? { runTimeoutMs: options.runTimeoutMs } : {}),
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
    void this.adapter.run();
    await this.waitForFrame((frame) => frame.method === "hello", "hello");
  }

  private send(line: string): void {
    this.pending.push(line);
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
  }

  async requestResult(method: string, params: unknown = {}): Promise<Record<string, unknown>> {
    const id = 100_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    const frame = await this.waitForFrame((candidate) => candidate.id === id, `response ${method}`);
    if (frame.error) throw new Error(`RPC error ${frame.error.code}: ${frame.error.message}`);
    return frame.result ?? {};
  }

  async requestError(method: string, params: unknown = {}): Promise<Frame> {
    const id = 200_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    return this.waitForFrame(
      (candidate) => candidate.id === id && candidate.error !== undefined,
      `error ${method}`,
    );
  }

  events(): Frame[] {
    return this.frames.filter((frame) => frame.method === "event");
  }

  runEvents(runId: string): Frame[] {
    return this.events().filter((frame) => frame.params?.run_id === runId);
  }

  runTypes(runId: string): string[] {
    return this.runEvents(runId).map((frame) => String(frame.params?.type));
  }

  async waitForFrame(predicate: (frame: Frame) => boolean, label = "frame"): Promise<Frame> {
    const deadline = Date.now() + 20_000;
    for (;;) {
      const found = this.frames.find(predicate);
      if (found) return found;
      if (Date.now() > deadline) {
        throw new Error(`等待 ${label} 超时；已收帧=${JSON.stringify(this.frames, null, 2)}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  waitForEvent(type: string, runId?: string): Promise<Frame> {
    return this.waitForFrame(
      (frame) =>
        frame.method === "event" &&
        frame.params?.type === type &&
        (runId === undefined || frame.params.run_id === runId),
      `event ${type}`,
    );
  }

  async waitTerminal(runId: string): Promise<string> {
    const terminal = ["run.completed", "run.failed", "run.cancelled"];
    const deadline = Date.now() + 20_000;
    for (;;) {
      const found = this.runEvents(runId).find((frame) =>
        terminal.includes(String(frame.params?.type)),
      );
      if (found) return String(found.params?.type);
      if (Date.now() > deadline) {
        throw new Error(
          `等待 run 终态超时；序列=${JSON.stringify(this.runTypes(runId))}；stderr=${JSON.stringify(this.stderr)}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  async createSession(params: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
    return this.requestResult("session.create", params);
  }

  async sendMessage(sessionId: string, text: string, clientMsgId?: string): Promise<string> {
    const result = await this.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: clientMsgId ?? `c-${Math.random().toString(36).slice(2)}`,
      text,
    });
    return String(result.run_id);
  }

  lastPid(): number | null {
    try {
      const lines = readFileSync(this.pidFile, "utf8").trim().split(/\r?\n/);
      const value = Number(lines[lines.length - 1]);
      return Number.isFinite(value) ? value : null;
    } catch {
      return null;
    }
  }
}

function pidAlive(pid: number): boolean {
  if (process.platform === "win32") {
    const out = spawnSync("tasklist", ["/FI", `PID eq ${pid}`, "/NH", "/FO", "CSV"], {
      encoding: "utf8",
      windowsHide: true,
    });
    return (out.stdout ?? "").includes(`"${pid}"`);
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitPidGone(pid: number, timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!pidAlive(pid)) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return !pidAlive(pid);
}

async function waitUntil(predicate: () => boolean, timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return predicate();
}

function payload(frame: Frame | undefined): Record<string, unknown> {
  return (frame?.params?.payload ?? {}) as Record<string, unknown>;
}

describe("ClaudeCodeAdapter：D6 方法（M2-02 DoD1/DoD2）", () => {
  it("hello + initialize + session.create 生成 UUID native_id；tools.list", { timeout: 30_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const hello = harness.frames.find((frame) => frame.method === "hello");
    expect(hello?.params?.["protocol"]).toBe("1.0");
    const runtime = hello?.params?.["runtime"] as Record<string, unknown> | undefined;
    expect(runtime?.["name"]).toBe("claude-code");
    expect(runtime?.["capabilities"]).toContain("session.send");

    const initialized = await harness.requestResult("initialize", { config: {} });
    expect(initialized["acknowledged"]).toBe(true);

    const created = await harness.createSession({ title: "t" });
    const nativeId = created["native_id"] as string;
    expect(created["session_id"]).toBe(nativeId);
    expect(created["resumed"]).toBe(false);
    expect(nativeId).toMatch(/^[0-9a-f-]{36}$/i);

    const tools = await harness.requestResult("tools.list", { session_id: nativeId });
    expect((tools["tools"] as unknown[]).length).toBeGreaterThan(0);

    const resolved = await harness.requestResult("permission.resolve", {
      request_id: "r-1",
      decision: "allow",
      scope: "once",
    });
    expect(resolved["resolved"]).toBe(false);
    expect(String(resolved["reason"])).toContain("D9 边界");
  });

  it("sendMessage 流式：ack 快路径 + delta 拼接 = 终稿 + seq 单调", { timeout: 30_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const { session_id: sessionId } = await harness.createSession();
    const started = Date.now();
    const runId = await harness.sendMessage(String(sessionId), "chat");
    expect(Date.now() - started).toBeLessThan(1000);

    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const types = harness.runTypes(runId);
    expect(types[0]).toBe("run.started");
    expect(types[types.length - 1]).toBe("run.completed");
    const deltas = harness.runEvents(runId).filter((frame) => frame.params?.type === "message.delta");
    expect(deltas.length).toBeGreaterThan(1);
    const concatenated = deltas
      .map((frame) => payload(frame)["text"] as string)
      .join("");
    const completed = harness
      .runEvents(runId)
      .find((frame) => frame.params?.type === "message.completed");
    const message = payload(completed)["message"] as Record<string, unknown>;
    expect(message["content"]).toBe(concatenated);
    expect(String(message["content"])).toContain("Aether M2-02 fake");

    const seqs = harness.runEvents(runId).map((frame) => Number(frame.params?.seq));
    expect(new Set(seqs).size).toBe(seqs.length);
    expect([...seqs].sort((a, b) => a - b)).toEqual(seqs);
  });

  it("同一 client_msg_id 重发不重复；不同消息可续聊", { timeout: 30_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const { session_id: sessionId } = await harness.createSession();
    const first = await harness.sendMessage(String(sessionId), "chat", "dup-1");
    const duplicate = await harness.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: "dup-1",
      text: "chat",
    });
    expect(duplicate["duplicate"]).toBe(true);
    expect(duplicate["run_id"]).toBe(first);
    await harness.waitTerminal(first);

    const second = await harness.sendMessage(String(sessionId), "chat", "dup-2");
    expect(second).not.toBe(first);
    expect(await harness.waitTerminal(second)).toBe("run.completed");
  });

  it("工具调用：正常完成 / 执行失败（附录 B 序列 + tools.list 观察子集）", { timeout: 30_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const { session_id: sessionId } = await harness.createSession();

    const normalRun = await harness.sendMessage(String(sessionId), "tool:normal");
    expect(await harness.waitTerminal(normalRun)).toBe("run.completed");
    expect(harness.runTypes(normalRun).filter((type) => type.startsWith("tool."))).toEqual([
      "tool.call_started",
      "tool.call_completed",
    ]);

    const failedRun = await harness.sendMessage(String(sessionId), "tool:fail");
    expect(await harness.waitTerminal(failedRun)).toBe("run.completed");
    expect(harness.runTypes(failedRun).filter((type) => type.startsWith("tool."))).toEqual([
      "tool.call_started",
      "tool.call_failed",
    ]);
    const failed = harness
      .runEvents(failedRun)
      .find((frame) => frame.params?.type === "tool.call_failed");
    const error = payload(failed)["error"] as Record<string, unknown>;
    expect(error["code"]).toBe("tool_execution_failed");
    expect(String(error["message"])).toContain("ENOENT");

    const tools = await harness.requestResult("tools.list", { session_id: sessionId });
    const names = (tools["tools"] as Array<Record<string, unknown>>).map((tool) => tool["name"]);
    expect(names).toContain("Bash");
    expect(names).toContain("Write");
  });

  it("interrupt：5s 内返回；工具收口 + run.cancelled；CLI 进程被回收", { timeout: 40_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const { session_id: sessionId } = await harness.createSession();
    const runId = await harness.sendMessage(String(sessionId), "tool:slow");
    await harness.waitForEvent("tool.call_started", runId);
    await waitUntil(() => harness.lastPid() !== null);
    const pid = harness.lastPid();
    expect(pid).not.toBeNull();

    const started = Date.now();
    const interrupted = await harness.requestResult("session.interrupt", {
      session_id: sessionId,
    });
    expect(Date.now() - started).toBeLessThan(5000);
    expect(interrupted["interrupted"]).toBe(true);
    expect(await harness.waitTerminal(runId)).toBe("run.cancelled");
    const toolFailed = harness
      .runEvents(runId)
      .find((frame) => frame.params?.type === "tool.call_failed");
    const error = payload(toolFailed)["error"] as Record<string, unknown>;
    expect(error["code"]).toBe("timeout");
    expect(String(error["message"])).toContain("abort");
    expect(await waitPidGone(pid as number)).toBe(true);

    const noActive = await harness.requestResult("session.interrupt", { session_id: sessionId });
    expect(noActive["interrupted"]).toBe(false);
  });

  it("异常路径：api 错误 / 无 result / 坏行不健康 / spawn 失败 / 会话与方法的错误码", { timeout: 90_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const { session_id: sessionId } = await harness.createSession();

    const apiRun = await harness.sendMessage(String(sessionId), "fail:api-error");
    expect(await harness.waitTerminal(apiRun)).toBe("run.failed");
    const apiError = payload(
      harness.runEvents(apiRun).find((frame) => frame.params?.type === "run.failed"),
    )["error"] as Record<string, unknown>;
    expect(apiError["code"]).toBe("api_error");
    expect(String(apiError["message"])).toContain("503");
    expect(apiError["recoverable"]).toBe(true);

    const noResult = await harness.sendMessage(String(sessionId), "fail:no-result");
    expect(await harness.waitTerminal(noResult)).toBe("run.failed");
    const exitError = payload(
      harness.runEvents(noResult).find((frame) => frame.params?.type === "run.failed"),
    )["error"] as Record<string, unknown>;
    expect(exitError["code"]).toBe("cli_exit");
    expect(String(exitError["message"])).toContain("exit=7");

    const badJson = await harness.sendMessage(String(sessionId), "fail:bad-json");
    expect(await harness.waitTerminal(badJson)).toBe("run.failed");
    const unhealthyError = payload(
      harness.runEvents(badJson).find((frame) => frame.params?.type === "run.failed"),
    )["error"] as Record<string, unknown>;
    expect(unhealthyError["code"]).toBe("cli_exit");
    expect(String(unhealthyError["message"])).toContain("20");

    const spawnHarness = new Harness({ cli: { bin: "definitely-missing-claude-binary-xyz" } as ClaudeCliConfig });
    await spawnHarness.start();
    const spawnSession = await spawnHarness.createSession();
    const spawnRun = await spawnHarness.sendMessage(String(spawnSession["session_id"]), "chat");
    expect(await spawnHarness.waitTerminal(spawnRun)).toBe("run.failed");
    const spawnError = payload(
      spawnHarness.runEvents(spawnRun).find((frame) => frame.params?.type === "run.failed"),
    )["error"] as Record<string, unknown>;
    expect(spawnError["code"]).toBe("spawn_failed");

    const missingSession = await harness.requestError("session.send", {
      session_id: "11111111-2222-3333-4444-555555555555",
      client_msg_id: "x",
      text: "chat",
    });
    expect(missingSession.error?.code).toBe(1005);

    const unknown = await harness.requestError("session.listen", {});
    expect(unknown.error?.code).toBe(-32601);

    const disposed = await harness.requestResult("session.dispose", { session_id: sessionId });
    expect(disposed["disposed"]).toBe(true);
    const afterDispose = await harness.requestError("session.send", {
      session_id: sessionId,
      client_msg_id: "after",
      text: "chat",
    });
    expect(afterDispose.error?.code).toBe(1005);
  });

  it("Mode R：跨适配器进程用 native_id 恢复原生会话并续聊", { timeout: 40_000 }, async () => {
    const first = new Harness();
    await first.start();
    const created = await first.createSession();
    const nativeId = String(created["session_id"]);
    const rememberRun = await first.sendMessage(nativeId, "remember:AETHER-M2-02-TOKEN");
    expect(await first.waitTerminal(rememberRun)).toBe("run.completed");
    await first.requestResult("session.dispose", { session_id: nativeId });

    // 模拟适配器进程重启：新实例（无内存会话表），仅凭 native_id 恢复。
    const second = new Harness();
    process.env.FAKE_CLAUDE_HOME = first.home;
    await second.start();
    const resumed = await second.createSession({ native_id: nativeId });
    expect(resumed["resumed"]).toBe(true);
    const recallRun = await second.sendMessage(nativeId, "recall");
    expect(await second.waitTerminal(recallRun)).toBe("run.completed");
    const completed = second
      .runEvents(recallRun)
      .find((frame) => frame.params?.type === "message.completed");
    const message = payload(completed)["message"] as Record<string, unknown>;
    expect(String(message["content"])).toBe("AETHER-M2-02-TOKEN");
  });

  it("native_id 非法 → invalid_params；shutdown 退出码 0", { timeout: 30_000 }, async () => {
    const harness = new Harness();
    await harness.start();
    const invalid = await harness.requestError("session.create", { native_id: "not-a-uuid" });
    expect(invalid.error?.code).toBe(-32602);

    await harness.requestResult("shutdown", {});
    await waitUntil(() => harness.exits.length > 0);
    expect(harness.exits).toContain(0);
  });
});
