import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { CodexAdapter, type CodexAdapterOptions } from "./codex-adapter";
import type { CodexCliConfig } from "./codex-cli";

const FAKE_CLI = fileURLToPath(
  new URL("../../../scripts/test/m2-11/fake-codex/cli.mjs", import.meta.url),
);

interface Frame {
  jsonrpc?: string;
  id?: number | string;
  method?: string;
  params?: Record<string, unknown> & { type?: string; run_id?: string };
  result?: Record<string, unknown>;
  error?: { code: number; message: string };
}

class Harness {
  readonly frames: Frame[] = [];
  readonly stderr: string[] = [];
  readonly adapter: CodexAdapter;
  readonly home: string;
  readonly stateDir: string;
  readonly pidFile: string;

  private readonly pending: string[] = [];
  private wake?: () => void;
  private readonly lines = this.lineStream();

  constructor(options: Partial<CodexAdapterOptions> = {}, home?: string) {
    this.home = home ?? mkdtempSync(join(tmpdir(), "fake-codex-"));
    this.stateDir = join(this.home, "aether-bridge");
    this.pidFile = join(this.home, "pids.txt");
    process.env.CODEX_HOME = this.home;
    process.env.FAKE_CODEX_PID_FILE = this.pidFile;
    const cli: CodexCliConfig = {
      bin: process.execPath,
      extraArgs: [FAKE_CLI],
      home: this.home,
      workspace: process.cwd(),
      sandbox: "read-only",
      reasoning: "low",
      ...(options.cli ?? {}),
    };
    this.adapter = new CodexAdapter({
      lines: this.lines,
      writeLine: async (line) => {
        this.frames.push(JSON.parse(line) as Frame);
      },
      stderr: (line) => {
        this.stderr.push(line);
      },
      exit: () => {},
      cli,
      stateDir: this.stateDir,
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

  private sendRaw(line: string): void {
    this.pending.push(line);
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
  }

  async requestResult(method: string, params: unknown = {}): Promise<Record<string, unknown>> {
    const id = 100_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.sendRaw(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    const frame = await this.waitForFrame((candidate) => candidate.id === id, `response ${method}`);
    if (frame.error) throw new Error(`RPC error ${frame.error.code}: ${frame.error.message}`);
    return frame.result ?? {};
  }

  async requestError(method: string, params: unknown = {}): Promise<Frame> {
    const id = 200_000 + this.frames.filter((frame) => frame.id !== undefined).length;
    this.sendRaw(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
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

  payload(frame: Frame | undefined): Record<string, unknown> {
    return (frame?.params?.payload ?? {}) as Record<string, unknown>;
  }

  async waitPid(): Promise<number> {
    const deadline = Date.now() + 10_000;
    for (;;) {
      const pid = this.lastPid();
      if (pid !== null) return pid;
      if (Date.now() > deadline) throw new Error("等待夹具 pid-file 超时");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
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
        throw new Error(`等待 run ${runId} 终态超时；事件=${JSON.stringify(this.runTypes(runId))}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  lastPid(): number | null {
    try {
      const lines = readFileSync(this.pidFile, "utf8").trim().split(/\r?\n/);
      const last = lines[lines.length - 1];
      return last ? Number(last) : null;
    } catch {
      return null;
    }
  }
}

async function createSession(harness: Harness, nativeId?: string): Promise<Record<string, unknown>> {
  return harness.requestResult("session.create", nativeId ? { native_id: nativeId } : {});
}

async function send(harness: Harness, sessionId: string, text: string, clientMsgId: string) {
  return harness.requestResult("session.send", {
    session_id: sessionId,
    client_msg_id: clientMsgId,
    text,
  });
}

describe("CodexAdapter（D6 方法实现）", () => {
  it("initialize / health.ping / shutdown 基础面", async () => {
    const harness = new Harness();
    await harness.start();
    const initialized = await harness.requestResult("initialize", {});
    expect(initialized.acknowledged).toBe(true);
    const pong = await harness.requestResult("health.ping");
    expect(pong.status).toBe("ok");
    const shutdown = await harness.requestResult("shutdown");
    expect(shutdown.ok).toBe(true);
  });

  it("session.create 返回 ULID 别名并支持缺省/覆盖模型", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);
    expect(String(session.session_id)).toMatch(/^[0-9A-HJKMNP-TV-Z]{26}$/);
    expect(session.native_id).toBe(session.session_id);
    expect(session.resumed).toBe(false);
    const bad = await harness.requestError("session.create", { native_id: "not-a-ulid" });
    expect(bad.error?.code).toBe(-32602);
  });

  it("流式 run：deltas 拼接等于 message.completed，含 usage", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);
    const ack = await send(harness, String(session.session_id), "chat", "m-codex-1");
    expect(ack.accepted).toBe(true);
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const types = harness.runTypes(runId);
    expect(types[0]).toBe("run.started");
    expect(types.filter((type) => type === "message.delta").length).toBeGreaterThan(0);

    const completed = harness.runEvents(runId).find(
      (frame) => frame.params?.type === "message.completed",
    );
    const message = harness.payload(completed)["message"] as Record<string, unknown>;
    const deltas = harness
      .runEvents(runId)
      .filter((frame) => frame.params?.type === "message.delta")
      .map((frame) => String(harness.payload(frame)["text"]))
      .join("");
    expect(message.content).toBe(deltas);
    expect(deltas).toContain("Aether M2-11 fake codex baseline");
    expect(harness.payload(completed)["usage"]).toMatchObject({
      input_tokens: 12,
      output_tokens: 34,
    });
  });

  it("stream:multi：item.updated 增量去重后拼接一致", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);
    const ack = await send(harness, String(session.session_id), "stream:multi", "m-codex-multi");
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const texts = harness
      .runEvents(runId)
      .filter((frame) => frame.params?.type === "message.delta")
      .map((frame) => String(harness.payload(frame)["text"]));
    expect(texts).toEqual(["part one", "part two"]);
    const completed = harness.runEvents(runId).find(
      (frame) => frame.params?.type === "message.completed",
    );
    const message = harness.payload(completed)["message"] as Record<string, unknown>;
    expect(message.content).toBe("part onepart two");
  });

  it("工具调用：正常完成与失败分别映射附录 B 事件", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);

    const normal = await send(harness, String(session.session_id), "tool:normal", "m-codex-tool-1");
    const normalId = String(normal.run_id);
    expect(await harness.waitTerminal(normalId)).toBe("run.completed");
    const normalTools = harness
      .runTypes(normalId)
      .filter((type) => type.startsWith("tool."));
    expect(normalTools).toEqual(["tool.call_started", "tool.call_completed"]);

    const failed = await send(harness, String(session.session_id), "tool:fail", "m-codex-tool-2");
    const failedId = String(failed.run_id);
    expect(await harness.waitTerminal(failedId)).toBe("run.completed");
    const failedEvent = harness
      .runEvents(failedId)
      .find((frame) => frame.params?.type === "tool.call_failed");
    const error = harness.payload(failedEvent)["error"] as Record<string, unknown>;
    expect(error.code).toBe("tool_execution_failed");
    expect(String(error.message)).toContain("failed");
    const tools = await harness.requestResult("tools.list", {
      session_id: session.session_id,
    });
    const names = (tools.tools as Array<{ name: string }>).map((tool) => tool.name);
    expect(names).toContain("command_execution");
    expect(names).toContain("file_change");
  });

  it("异常路径：turn.failed / 无终态退出 / 连续坏行", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);

    const turnFail = await send(harness, String(session.session_id), "fail:turn", "m-codex-f1");
    expect(await harness.waitTerminal(String(turnFail.run_id))).toBe("run.failed");
    const turnEvent = harness
      .runEvents(String(turnFail.run_id))
      .find((frame) => frame.params?.type === "run.failed");
    expect((harness.payload(turnEvent)["error"] as Record<string, unknown>).code).toBe(
      "turn_failed",
    );

    const orphan = await send(harness, String(session.session_id), "fail:no-terminal", "m-codex-f2");
    expect(await harness.waitTerminal(String(orphan.run_id))).toBe("run.failed");
    const orphanEvent = harness
      .runEvents(String(orphan.run_id))
      .find((frame) => frame.params?.type === "run.failed");
    expect((harness.payload(orphanEvent)["error"] as Record<string, unknown>).code).toBe(
      "cli_exit",
    );
    expect(
      String((harness.payload(orphanEvent)["error"] as Record<string, unknown>).message),
    ).toContain("exit=7");

    const unhealthy = await send(harness, String(session.session_id), "fail:bad-json", "m-codex-f3");
    expect(await harness.waitTerminal(String(unhealthy.run_id))).toBe("run.failed");
    const unhealthyEvent = harness
      .runEvents(String(unhealthy.run_id))
      .find((frame) => frame.params?.type === "run.failed");
    expect(
      String((harness.payload(unhealthyEvent)["error"] as Record<string, unknown>).message),
    ).toContain("20");
  }, 30_000);

  it("interrupt：run.cancelled + CLI 进程树回收", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);
    const ack = await send(harness, String(session.session_id), "slow", "m-codex-slow");
    await harness.waitForFrame(
      (frame) => frame.params?.type === "run.started" && frame.params?.run_id === ack.run_id,
      "run.started",
    );
    const pid = await harness.waitPid();
    expect(pid).toBeGreaterThan(0);
    const interrupted = await harness.requestResult("session.interrupt", {
      session_id: session.session_id,
    });
    expect(interrupted.interrupted).toBe(true);
    expect(await harness.waitTerminal(String(ack.run_id))).toBe("run.cancelled");
    const again = await harness.requestResult("session.interrupt", {
      session_id: session.session_id,
    });
    expect(again.interrupted).toBe(false);
  });

  it("幂等（client_msg_id）与 dispose 后拒绝", async () => {
    const harness = new Harness();
    await harness.start();
    const session = await createSession(harness);
    const first = await send(harness, String(session.session_id), "chat", "m-codex-dup");
    const duplicate = await send(harness, String(session.session_id), "chat", "m-codex-dup");
    expect(duplicate.duplicate).toBe(true);
    expect(duplicate.run_id).toBe(first.run_id);
    await harness.waitTerminal(String(first.run_id));

    const disposed = await harness.requestResult("session.dispose", {
      session_id: session.session_id,
    });
    expect(disposed.disposed).toBe(true);
    const after = await harness.requestError("session.send", {
      session_id: session.session_id,
      client_msg_id: "m-codex-after",
      text: "chat",
    });
    expect(after.error?.code).toBe(1005);
  });

  it("permission.resolve 声明 D9 边界（不伪造回环）", async () => {
    const harness = new Harness();
    await harness.start();
    const resolved = await harness.requestResult("permission.resolve", {
      request_id: "req-1",
      decision: "allow",
    });
    expect(resolved.resolved).toBe(false);
    expect(String(resolved.reason)).toContain("D9 边界");
  });

  it("Mode R：别名映射跨适配器进程恢复（exec resume）", async () => {
    const home = mkdtempSync(join(tmpdir(), "fake-codex-mode-r-"));
    const first = new Harness({}, home);
    await first.start();
    const session = await createSession(first);
    const nativeId = String(session.native_id);
    const remember = await send(first, String(session.session_id), "remember:AETHER-CODEX-TOKEN", "m-codex-r1");
    expect(await first.waitTerminal(String(remember.run_id))).toBe("run.completed");

    // 模拟适配器进程重启：同一 CODEX_HOME/stateDir 新建适配器。
    const second = new Harness({}, home);
    await second.start();
    const resumed = await createSession(second, nativeId);
    expect(resumed.resumed).toBe(true);
    expect(resumed.session_id).toBe(nativeId);
    const recall = await send(second, nativeId, "recall", "m-codex-r2");
    expect(await second.waitTerminal(String(recall.run_id))).toBe("run.completed");
    const completed = second
      .runEvents(String(recall.run_id))
      .find((frame) => frame.params?.type === "message.completed");
    const message = second.payload(completed)["message"] as Record<string, unknown>;
    expect(message.content).toBe("AETHER-CODEX-TOKEN");
  });
});

describe("CodexSessionStore", () => {
  it("损坏状态文件按空映射继续（诊断可见）", async () => {
    const home = mkdtempSync(join(tmpdir(), "fake-codex-store-"));
    const stateDir = join(home, "aether-bridge");
    mkdirSync(stateDir, { recursive: true });
    writeFileSync(join(stateDir, "sessions.json"), "{ 不是 JSON", "utf8");
    const harness = new Harness({}, home);
    await harness.start();
    const session = await createSession(harness);
    expect(session.resumed).toBe(false);
    expect(harness.stderr.some((line) => line.includes("别名映射加载失败"))).toBe(true);
  });
});
