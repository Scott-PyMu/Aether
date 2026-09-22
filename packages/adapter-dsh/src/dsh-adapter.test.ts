import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { DshAdapter, type DshAdapterOptions } from "./dsh-adapter";
import { DEFAULT_DSH_VERSION_PIN } from "./dsh-plugin";

const FAKE_ACP = fileURLToPath(
  new URL("../../../scripts/test/m2-11/fake-dsh/acp-server.mjs", import.meta.url),
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
  readonly adapter: DshAdapter;
  readonly home: string;
  readonly workspace: string;
  readonly pidFile: string;

  private readonly pending: string[] = [];
  private wake?: () => void;
  private readonly lines = this.lineStream();

  constructor(options: Partial<DshAdapterOptions> = {}, home?: string) {
    this.home = home ?? mkdtempSync(join(tmpdir(), "fake-dsh-"));
    this.workspace = mkdtempSync(join(tmpdir(), "fake-dsh-ws-"));
    this.pidFile = join(this.home, "pids.txt");
    process.env.FAKE_DSH_PID_FILE = this.pidFile;
    this.adapter = new DshAdapter({
      lines: this.lines,
      writeLine: async (line) => {
        this.frames.push(JSON.parse(line) as Frame);
      },
      stderr: (line) => {
        this.stderr.push(line);
      },
      exit: () => {},
      cli: {
        bin: FAKE_ACP,
        nodeBin: process.execPath,
        home: this.home,
        profile: "acp",
        provider: "fake",
        model: "fake-model",
        extraArgs: [],
        workspace: this.workspace,
        versionPin: DEFAULT_DSH_VERSION_PIN,
        dshVersion: DEFAULT_DSH_VERSION_PIN,
        ...(options.cli ?? {}),
      },
      ...(options.runTimeoutMs !== undefined ? { runTimeoutMs: options.runTimeoutMs } : {}),
      ...(options.permissionTimeoutMs !== undefined
        ? { permissionTimeoutMs: options.permissionTimeoutMs }
        : {}),
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

  private nextId(offset: number): number {
    return offset + this.frames.filter((frame) => frame.id !== undefined).length;
  }

  async requestResult(method: string, params: unknown = {}): Promise<Record<string, unknown>> {
    const id = this.nextId(100_000);
    this.sendRaw(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    const frame = await this.waitForFrame((candidate) => candidate.id === id, `response ${method}`);
    if (frame.error) throw new Error(`RPC error ${frame.error.code}: ${frame.error.message}`);
    return frame.result ?? {};
  }

  async requestError(method: string, params: unknown = {}): Promise<Frame> {
    const id = this.nextId(200_000);
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

  payload(frame: Frame | undefined): Record<string, unknown> {
    return (frame?.params?.payload ?? {}) as Record<string, unknown>;
  }

  deltaText(runId: string): string {
    return this.runEvents(runId)
      .filter((frame) => frame.params?.type === "message.delta")
      .map((frame) => String(this.payload(frame)["text"] ?? ""))
      .join("");
  }

  completedText(runId: string): string {
    const frame = this.runEvents(runId).find(
      (candidate) => candidate.params?.type === "message.completed",
    );
    const message = this.payload(frame)["message"] as Record<string, unknown> | undefined;
    return String(message?.content ?? "");
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

  async waitPermissionRequest(count = 1): Promise<Record<string, unknown>> {
    const frame = await this.waitForFrame(
      () =>
        this.frames.filter((candidate) => candidate.method === "permission.request").length >=
        count,
      `permission.request #${count}`,
    );
    void frame;
    const all = this.frames.filter((candidate) => candidate.method === "permission.request");
    return all[all.length - 1]?.params ?? {};
  }

  async createSession(nativeId?: string): Promise<Record<string, unknown>> {
    return this.requestResult("session.create", nativeId ? { native_id: nativeId } : {});
  }

  async send(sessionId: string, text: string, clientMsgId: string) {
    return this.requestResult("session.send", {
      session_id: sessionId,
      client_msg_id: clientMsgId,
      text,
    });
  }

  async resolvePermission(requestId: string, decision: "allow" | "deny"): Promise<void> {
    await this.requestResult("permission.resolve", { request_id: requestId, decision });
  }

  async shutdown(): Promise<void> {
    await this.requestResult("shutdown").catch(() => undefined);
    await new Promise((resolve) => setTimeout(resolve, 300));
  }
}

async function initializedHarness(options: Partial<DshAdapterOptions> = {}) {
  const harness = new Harness(options);
  await harness.start();
  const initialized = await harness.requestResult("initialize", {});
  expect(initialized.acknowledged).toBe(true);
  return { harness, initialized };
}

describe("DshAdapter initialize / 版本门闩 / 注入（DoD1/2）", () => {
  it("initialize 返回 dsh 版本与插件契约；health.ping 两态", async () => {
    const { harness, initialized } = await initializedHarness();
    expect(initialized.dsh_version).toBe(DEFAULT_DSH_VERSION_PIN);
    expect(initialized.plugin_contract).toBe("aether-dsh-stream@1");
    const pong = await harness.requestResult("health.ping");
    expect(pong.status).toBe("ok");
    expect(pong.plugin_contract).toBe("aether-dsh-stream@1");
    expect(pong.dsh_pid).toBeTypeOf("number");

    // 插件包已落盘且无 BOM。
    const pluginPkg = join(
      harness.home,
      "profiles",
      "acp",
      "node_modules",
      "aether-dsh-stream",
      "package.json",
    );
    expect(existsSync(pluginPkg)).toBe(true);
    const bytes = readFileSync(pluginPkg);
    expect([bytes[0], bytes[1], bytes[2]]).not.toEqual([0xef, 0xbb, 0xbf]);
    const patch = readFileSync(join(harness.home, "aether-dsh-acp.patch.yml"), "utf8");
    expect(patch).toContain("insert");
    expect(patch).toContain("aether-dsh-stream");
    await harness.shutdown();
  });

  it("版本不匹配 → initialize 返回 1003 + 升级提示（disabled + version_mismatch 输入）", async () => {
    const harness = new Harness({ cli: { dshVersion: "0.1.1-rc.2" } as never });
    await harness.start();
    const error = await harness.requestError("initialize", {});
    expect(error.error?.code).toBe(1003);
    expect(String(error.error?.message)).toContain(DEFAULT_DSH_VERSION_PIN);
    await harness.shutdown();
  });

  it("插件契约帧缺失（帧形状探针失败）→ 1003", async () => {
    process.env.FAKE_DSH_NO_PLUGIN_HELLO = "1";
    try {
      const harness = new Harness({ cli: { helloTimeoutMs: 300 } as never });
      await harness.start();
      const error = await harness.requestError("initialize", {});
      expect(error.error?.code).toBe(1003);
      expect(String(error.error?.message)).toContain("插件契约帧");
      await harness.shutdown();
    } finally {
      delete process.env.FAKE_DSH_NO_PLUGIN_HELLO;
    }
  });
});

describe("DshAdapter provider 路由就绪（冷启动竞态兜底）", () => {
  it("前 2 次 session/new 报 no adapter registered → 有界重试后成功", async () => {
    process.env.FAKE_DSH_NEW_FAILS = "2";
    try {
      const { harness } = await initializedHarness();
      const session = await harness.createSession();
      expect(String(session.session_id)).toMatch(/^acp-/);
      expect(harness.stderr.some((line) => line.includes("等待重试"))).toBe(true);
      await harness.shutdown();
    } finally {
      delete process.env.FAKE_DSH_NEW_FAILS;
    }
  });

  it("路由持续缺失 → 重试预算耗尽后失败（不无限阻塞）", async () => {
    process.env.FAKE_DSH_NEW_FAILS = "999";
    try {
      const { harness } = await initializedHarness();
      await expect(harness.createSession()).rejects.toThrow(/no adapter registered/);
      await harness.shutdown();
    } finally {
      delete process.env.FAKE_DSH_NEW_FAILS;
    }
  }, 20_000);
});

describe("DshAdapter 流式 / 去重 / 兜底（DoD3/5）", () => {
  it("默认路径：token 级 delta 拼接等于 message.completed", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "chat", "m-dsh-1");
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const types = harness.runTypes(runId);
    expect(types[0]).toBe("run.started");
    expect(types.filter((type) => type === "message.delta").length).toBeGreaterThan(3);
    const deltas = harness.deltaText(runId);
    expect(deltas).toBe(harness.completedText(runId));
    expect(deltas).toContain("Aether M2-11 fake dsh baseline");
    await harness.shutdown();
  });

  it("dedupe:suffix：final-only 后缀作为增量补发，无重复", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "dedupe:suffix", "m-dsh-2");
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const deltas = harness.deltaText(runId);
    expect(deltas).toBe(harness.completedText(runId));
    expect(deltas).toContain("final-only suffix");
    await harness.shutdown();
  });

  it("no-end：丢弃 end 帧不影响去重与终稿", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "no-end", "m-dsh-3");
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    expect(harness.deltaText(runId)).toBe(harness.completedText(runId));
    await harness.shutdown();
  });

  it("drop-frames：通道截断 → 丢弃增量并按 ACP final 重建", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "drop-frames", "m-dsh-4");
    const runId = String(ack.run_id);
    expect(await harness.waitTerminal(runId)).toBe("run.completed");
    const completed = harness.completedText(runId);
    expect(completed).toContain("Aether M2-11 fake dsh dedupe");
    // 兜底：部分流式已上报，但终稿以 ACP final 为准（可能不等于 delta 拼接）。
    expect(harness.deltaText(runId)).not.toBe(completed);
    await harness.shutdown();
  });
});

describe("DshAdapter 工具 / 权限回环（DoD4）", () => {
  it("tool:normal / tool:fail 映射附录 B 工具事件", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();

    const normal = await harness.send(String(session.session_id), "tool:normal", "m-dsh-t1");
    expect(await harness.waitTerminal(String(normal.run_id))).toBe("run.completed");
    const normalTools = harness
      .runTypes(String(normal.run_id))
      .filter((type) => type.startsWith("tool."));
    expect(normalTools).toEqual(["tool.call_started", "tool.call_completed"]);

    const failed = await harness.send(String(session.session_id), "tool:fail", "m-dsh-t2");
    expect(await harness.waitTerminal(String(failed.run_id))).toBe("run.completed");
    const failedEvent = harness
      .runEvents(String(failed.run_id))
      .find((frame) => frame.params?.type === "tool.call_failed");
    expect((harness.payload(failedEvent)["error"] as Record<string, unknown>).code).toBe(
      "tool_execution_failed",
    );
    await harness.shutdown();
  });

  it("permission：100% 经 permission.request 回环；allow/deny 决议映射工具终态", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();

    const allow = harness.send(
      String(session.session_id),
      "permission:allow",
      "m-dsh-p1",
    );
    const request = await harness.waitPermissionRequest();
    expect(request.session_id).toBe(session.session_id);
    expect(request.tool_name).toBe("edit");
    // M2-10 冻结形状：resource/action/target 必须齐备（PermissionLoopRequest::from_params）。
    expect(request.resource).toBe("fs.write");
    expect(request.action).toBe("write");
    expect(request.target).toBe("a.txt");
    await harness.resolvePermission(String(request.request_id), "allow");
    const allowAck = await allow;
    expect(await harness.waitTerminal(String(allowAck.run_id))).toBe("run.completed");
    const allowTools = harness
      .runTypes(String(allowAck.run_id))
      .filter((type) => type.startsWith("tool."));
    expect(allowTools).toEqual(["tool.call_started", "tool.call_completed"]);

    const deny = harness.send(String(session.session_id), "permission:deny", "m-dsh-p2");
    const request2 = await harness.waitPermissionRequest(2);
    expect(request2.request_id).not.toBe(request.request_id);
    await harness.resolvePermission(String(request2.request_id), "deny");
    const denyAck = await deny;
    expect(await harness.waitTerminal(String(denyAck.run_id))).toBe("run.completed");
    const denyFailed = harness
      .runEvents(String(denyAck.run_id))
      .find((frame) => frame.params?.type === "tool.call_failed");
    expect(denyFailed).toBeDefined();

    // 零直通断言：两个 ask 场景各产生 1 条 permission.request 通知。
    const permissionRequests = harness.frames.filter(
      (frame) => frame.method === "permission.request",
    );
    expect(permissionRequests.length).toBe(2);
    await harness.shutdown();
  });

  it("权限超时 → 默认 deny（300s 口径的适配器侧兜底，压测注入 200ms）", async () => {
    const { harness } = await initializedHarness({ permissionTimeoutMs: 200 });
    const session = await harness.createSession();
    const ack = harness.send(String(session.session_id), "permission:deny", "m-dsh-p3");
    await harness.waitPermissionRequest();
    // 不决议，等待超时。
    const settled = await harness.waitTerminal(String((await ack).run_id));
    expect(settled).toBe("run.completed");
    const failed = harness
      .runEvents(String((await ack).run_id))
      .find((frame) => frame.params?.type === "tool.call_failed");
    expect(failed).toBeDefined();
    await harness.shutdown();
  });
});

describe("DshAdapter 中断 / dispose / Mode R", () => {
  it("session.interrupt：协议级 cancel 5s 内 settle → run.cancelled，无残留通道", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "cancel:long", "m-dsh-c1");
    const started = Date.now();
    const interrupted = await harness.requestResult("session.interrupt", {
      session_id: session.session_id,
    });
    expect(interrupted.interrupted).toBe(true);
    expect(Date.now() - started).toBeLessThan(5_000);
    expect(await harness.waitTerminal(String(ack.run_id))).toBe("run.cancelled");
    expect(harness.adapter.residualChannels()).toBe(0);
    const again = await harness.requestResult("session.interrupt", {
      session_id: session.session_id,
    });
    expect(again.interrupted).toBe(false);
    await harness.shutdown();
  });

  it("session.dispose：残留通道数 0（DoD6 探针）", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const ack = await harness.send(String(session.session_id), "chat", "m-dsh-d1");
    await harness.waitTerminal(String(ack.run_id));
    const disposed = await harness.requestResult("session.dispose", {
      session_id: session.session_id,
    });
    expect(disposed.disposed).toBe(true);
    expect(disposed.residual_channels).toBe(0);
    expect(harness.adapter.residualChannels()).toBe(0);
    const after = await harness.requestError("session.send", {
      session_id: session.session_id,
      client_msg_id: "m-dsh-d2",
      text: "chat",
    });
    expect(after.error?.code).toBe(1005);
    await harness.shutdown();
  });

  it("幂等（client_msg_id）", async () => {
    const { harness } = await initializedHarness();
    const session = await harness.createSession();
    const first = await harness.send(String(session.session_id), "chat", "m-dsh-dup");
    const duplicate = await harness.send(String(session.session_id), "chat", "m-dsh-dup");
    expect(duplicate.duplicate).toBe(true);
    expect(duplicate.run_id).toBe(first.run_id);
    await harness.shutdown();
  });

  it("Mode R：native_id 跨适配器进程 session/resume 恢复", async () => {
    const home = mkdtempSync(join(tmpdir(), "fake-dsh-mode-r-"));
    const first = new Harness({}, home);
    await first.start();
    await first.requestResult("initialize", {});
    const session = await first.createSession();
    const nativeId = String(session.native_id);
    const remember = await first.send(
      String(session.session_id),
      "remember:AETHER-DSH-TOKEN",
      "m-dsh-r1",
    );
    expect(await first.waitTerminal(String(remember.run_id))).toBe("run.completed");
    await first.shutdown();

    const second = new Harness({}, home);
    await second.start();
    await second.requestResult("initialize", {});
    const resumed = await second.createSession(nativeId);
    expect(resumed.resumed).toBe(true);
    expect(resumed.session_id).toBe(nativeId);
    const recall = await second.send(nativeId, "recall", "m-dsh-r2");
    expect(await second.waitTerminal(String(recall.run_id))).toBe("run.completed");
    expect(second.completedText(String(recall.run_id))).toBe("AETHER-DSH-TOKEN");
    await second.shutdown();
  });
});
