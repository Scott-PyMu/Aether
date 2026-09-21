import { describe, expect, it } from "vitest";

import { Adapter } from "./adapter";

function makeAdapter(options: {
  lines: string[];
  sendHello?: boolean;
  invalidFrameThreshold?: number;
  handler?: Record<string, (params: unknown) => unknown>;
}) {
  const written: string[] = [];
  const stderr: string[] = [];
  const adapter = new Adapter({
    runtime: { name: "unit", version: "0.1.0", capabilities: ["tools.list"] },
    lines: (async function* () {
      for (const line of options.lines) yield line;
    })(),
    writeLine: async (line) => {
      written.push(line);
    },
    stderr: (line) => {
      stderr.push(line);
    },
    sendHello: options.sendHello,
    invalidFrameThreshold: options.invalidFrameThreshold,
  });
  for (const [method, handler] of Object.entries(options.handler ?? {})) {
    adapter.handle(method, handler);
  }
  return { adapter, written, stderr };
}

describe("Adapter（D6）", () => {
  it("run 先发 hello（protocol 1.0 + runtime 信息）", async () => {
    const { adapter, written } = makeAdapter({ lines: [] });
    await adapter.run();
    const hello = JSON.parse(written[0]!);
    expect(hello.method).toBe("hello");
    expect(hello.params.protocol).toBe("1.0");
    expect(hello.params.runtime).toEqual({
      name: "unit",
      version: "0.1.0",
      capabilities: ["tools.list"],
    });
  });

  it("sendHello=false 时不发 hello（握手超时注入）", async () => {
    const { adapter, written } = makeAdapter({ lines: [], sendHello: false });
    await adapter.run();
    expect(written).toHaveLength(0);
  });

  it("已注册方法返回 result；未知方法 -32601", async () => {
    const { adapter, written } = makeAdapter({
      lines: [
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}',
        '{"jsonrpc":"2.0","id":2,"method":"session.listen","params":{}}',
      ],
      handler: { initialize: () => ({ capabilities: ["tools.list"] }) },
    });
    await adapter.run();
    const frames = written.map((line) => JSON.parse(line));
    expect(frames[1]).toEqual({ jsonrpc: "2.0", id: 1, result: { capabilities: ["tools.list"] } });
    expect(frames[2].error.code).toBe(-32601);
    expect(adapter.invalidFrameCount.streak).toBe(0);
  });

  it("emitEvent：信封字段与 seq 单调", async () => {
    const { adapter, written } = makeAdapter({ lines: [] });
    await adapter.run();
    await adapter.emitEvent(
      { runtimeId: "mock", sessionId: "s-1", runId: "r-1" },
      "tool.call_started",
      { tool_call_id: "t-1", tool_name: "mock", args: {} },
    );
    await adapter.emitEvent({ runtimeId: "mock", sessionId: "s-1", runId: "r-1" }, "log", {
      level: "info",
      message: "x",
    });
    await adapter.emitEvent({ runtimeId: "mock", sessionId: "s-2" }, "log", {
      level: "info",
      message: "y",
    });
    const [first, second, third] = written.slice(1).map((line) => JSON.parse(line));
    expect(first.method).toBe("event");
    expect(first.params.type).toBe("tool.call_started");
    expect(first.params.seq).toBe(1);
    expect(second.params.seq).toBe(2);
    expect(third.params.seq).toBe(1);
  });

  it("permission.request / log 通知形状", async () => {
    const { adapter, written } = makeAdapter({ lines: [] });
    await adapter.run();
    await adapter.emitPermissionRequest({ request_id: "p-1", resource: "fs.write" });
    await adapter.emitLog({ level: "info", message: "hi" });
    const [permission, log] = written.slice(1).map((line) => JSON.parse(line));
    expect(permission.method).toBe("permission.request");
    expect(permission.params.request_id).toBe("p-1");
    expect(log.method).toBe("log");
  });

  it("emitArtifactRef：M2-09 全帧形状（method/type 双判别键 + 路径/元数据）", async () => {
    const { adapter, written } = makeAdapter({ lines: [] });
    await adapter.run();
    await adapter.emitArtifactRef({
      session_id: "01JTEST",
      run_id: "01JRUN",
      refs: [{ path: "shot.png", size: 3145728, kind: "image/png" }],
    });
    const frame = JSON.parse(written[1]!);
    expect(frame).toEqual({
      jsonrpc: "2.0",
      method: "artifact_ref",
      type: "artifact_ref",
      params: {
        session_id: "01JTEST",
        run_id: "01JRUN",
        refs: [{ path: "shot.png", size: 3145728, kind: "image/png" }],
      },
    });
    // 引用帧必须 <1MiB（D6 契约；3MiB 附件只存 artifacts 文件）。
    expect(written[1]!.length).toBeLessThan(1024 * 1024);
  });

  it("连续 20 次无效帧停止处理（D6 硬阈值）", async () => {
    const lines = Array.from({ length: 25 }, (_, index) => `bad line ${index}`);
    const { adapter, stderr } = makeAdapter({ lines });
    await adapter.run();
    expect(adapter.invalidFrameCount.total).toBe(20);
    expect(adapter.invalidFrameCount.streak).toBe(20);
    expect(stderr.some((line) => line.includes("阈值"))).toBe(true);
  });

  it("有效帧重置连续计数", async () => {
    const lines = [
      "bad 1",
      "bad 2",
      '{"jsonrpc":"2.0","method":"log","params":{}}',
      "bad 3",
      "bad 4",
    ];
    const { adapter } = makeAdapter({ lines });
    await adapter.run();
    expect(adapter.invalidFrameCount.total).toBe(4);
    expect(adapter.invalidFrameCount.streak).toBe(2);
  });

  it("stop() 终止读循环", async () => {
    const written: string[] = [];
    let adapter!: Adapter;
    const lines = (async function* () {
      yield '{"jsonrpc":"2.0","id":1,"method":"health.ping","params":{}}';
      adapter.stop();
      yield "bad";
    })();
    adapter = new Adapter({
      runtime: { name: "unit", version: "0.1.0" },
      lines,
      writeLine: async (line) => {
        written.push(line);
      },
    });
    adapter.handle("health.ping", () => ({ status: "ok" }));
    await adapter.run();
    expect(written).toHaveLength(2);
  });
});
