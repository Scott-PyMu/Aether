import { describe, expect, it, vi } from "vitest";

import {
  AetherError,
  COMMANDS,
  createAetherClient,
  tryAetherError,
  type AetherBridge,
  type AetherCommand,
  type AetherRpcEnvelope,
} from "./client";
import type { AetherEvent } from "./types";

function makeBridge(): AetherBridge & {
  rpc: ReturnType<typeof vi.fn>;
  onEvent: ReturnType<typeof vi.fn>;
  offEvent: ReturnType<typeof vi.fn>;
  pathForFile: ReturnType<typeof vi.fn>;
  version: ReturnType<typeof vi.fn>;
} {
  return {
    rpc: vi.fn(async (): Promise<AetherRpcEnvelope> => ({ ok: true, result: null })),
    onEvent: vi.fn(),
    offEvent: vi.fn(),
    pathForFile: vi.fn(() => "C:/tmp/file.txt"),
    version: vi.fn(() => ({ app: "0.1.0", daemon: "0.1.0", core: "0.1.0" })),
  };
}

describe("@aether/protocol typed client（M9-08）", () => {
  it("恰好 63 个命名方法，且与 COMMANDS 一一对应", () => {
    const client = createAetherClient(makeBridge());
    const reserved = new Set(["rpc", "onEvent", "version"]);
    const methods = Object.keys(client).filter(
      (key) =>
        !reserved.has(key) &&
        typeof (client as unknown as Record<string, unknown>)[key] === "function",
    );
    expect(methods).toHaveLength(63);
    expect(COMMANDS).toHaveLength(63);
    expect(new Set(COMMANDS).size).toBe(63);
    for (const command of COMMANDS) {
      const method = command.replace(/_([a-z0-9])/g, (_, char: string) =>
        char.toUpperCase(),
      );
      expect(methods).toContain(method);
    }
  });

  it("命名方法把内层 DTO 作为 params 传给 preload（不再包 payload）", async () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    bridge.rpc.mockResolvedValueOnce({
      ok: true,
      result: [{ id: "session-1" }],
    });
    const params = { runtime_id: "mock", limit: 10 };
    await expect(client.sessionList<{ id: string }[]>(params)).resolves.toEqual([
      { id: "session-1" },
    ]);
    expect(bridge.rpc).toHaveBeenCalledWith("session_list", params);
  });

  it("无参方法省略 params（rpc 收到 undefined）", async () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    await client.runtimesList();
    expect(bridge.rpc).toHaveBeenCalledWith("runtimes_list", undefined);
  });

  it("失败信封转换为 AetherError；business_code 优先", async () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    bridge.rpc.mockResolvedValueOnce({
      ok: false,
      error: {
        code: -32000,
        message: "会话正在运行",
        data: {
          aether_code: "invalid_value",
          business_code: "session_busy",
          details: { count: 1 },
        },
      },
    });
    const failure = (await client.sessionSend({} as never).catch(
      (error) => error,
    )) as AetherError;
    expect(failure).toBeInstanceOf(AetherError);
    expect(failure.aether_code).toBe("invalid_value");
    expect(failure.business_code).toBe("session_busy");
    expect(failure.code).toBe("invalid_value");
    expect(failure.primary_code).toBe("session_busy");
    expect(failure.details).toEqual({ count: 1 });
    expect(failure.message).toBe("会话正在运行");
  });

  it("AetherError.from 兼容旧 code 对象、JSON-RPC 数值码与普通错误", () => {
    expect(
      AetherError.from({ code: "core_not_ready", message: "未就绪" }).primary_code,
    ).toBe("core_not_ready");
    expect(AetherError.from({ code: -32002, message: "未就绪" }).aether_code).toBe(
      "core_not_ready",
    );
    expect(AetherError.from(new Error("boom")).aether_code).toBe("internal");
    expect(tryAetherError("plain")).toBeNull();
  });

  it("onEvent 转发事件，解除函数调用 offEvent", () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    const handler = vi.fn();
    const off = client.onEvent(handler);
    expect(bridge.onEvent).toHaveBeenCalledWith(handler);
    off();
    expect(bridge.offEvent).toHaveBeenCalledWith(handler);

    const event: AetherEvent = {
      v: 1,
      id: "01J00000000000000000000001",
      session_id: "01J0000000000000000000000A",
      run_id: null,
      runtime_id: "mock",
      seq: 1,
      ts: 1_700_000_000_000,
      type: "session.updated",
      payload: {},
    };
    const registered = bridge.onEvent.mock.calls[0]?.[0] as (value: AetherEvent) => void;
    registered(event);
    expect(handler).toHaveBeenCalledWith(event);
  });

  it("未知方法被 client 拒绝（不进入 preload）", async () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    await expect(client.rpc("not_a_command" as AetherCommand)).rejects.toMatchObject({
      aether_code: "invalid_enum",
    });
    expect(bridge.rpc).not.toHaveBeenCalled();
  });

  it("version 透传 preload 版本信息", async () => {
    const bridge = makeBridge();
    const client = createAetherClient(bridge);
    await expect(await client.version()).toMatchObject({
      app: "0.1.0",
      daemon: "0.1.0",
      core: "0.1.0",
    });
  });
});
