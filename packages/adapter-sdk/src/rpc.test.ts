import { describe, expect, it, vi } from "vitest";

import { JsonRpcPeer, RpcError } from "./rpc";

function memoryPeer(handler?: (method: string, params: unknown) => unknown) {
  const written: string[] = [];
  const requests: Array<{ method: string; params: unknown }> = [];
  const notifications: Array<{ method: string; params: unknown }> = [];
  const responses: Array<{ id: number | string; result: unknown }> = [];
  const peer = new JsonRpcPeer(
    async (line) => {
      written.push(line);
    },
    {
      onRequest: (method, params) => {
        requests.push({ method, params });
        return handler?.(method, params) ?? null;
      },
      onNotification: (method, params) => {
        notifications.push({ method, params });
      },
      onResponse: (id, result) => {
        responses.push({ id, result });
      },
    },
  );
  return { peer, written, requests, notifications, responses };
}

describe("JsonRpcPeer", () => {
  it("发送通知帧", async () => {
    const { peer, written } = memoryPeer();
    await peer.notify("event", { a: 1 });
    expect(JSON.parse(written[0]!)).toEqual({
      jsonrpc: "2.0",
      method: "event",
      params: { a: 1 },
    });
    await peer.notify("hello");
    expect(JSON.parse(written[1]!)).toEqual({ jsonrpc: "2.0", method: "hello" });
  });

  it("请求 → result；RpcError → error 帧", async () => {
    const { peer, written, requests } = memoryPeer((method) => {
      if (method === "boom") throw new RpcError(-32601, "Method not found: boom", { m: "boom" });
      return { ok: true };
    });
    expect(await peer.handleLine('{"jsonrpc":"2.0","id":1,"method":"ping"}')).toBe(true);
    expect(requests).toEqual([{ method: "ping", params: null }]);
    expect(JSON.parse(written[0]!)).toEqual({ jsonrpc: "2.0", id: 1, result: { ok: true } });

    await peer.handleLine('{"jsonrpc":"2.0","id":"abc","method":"boom","params":{"x":1}}');
    expect(JSON.parse(written[1]!)).toEqual({
      jsonrpc: "2.0",
      id: "abc",
      error: { code: -32601, message: "Method not found: boom", data: { m: "boom" } },
    });
  });

  it("处理器普通异常 → -32603 且不断连", async () => {
    const { peer, written } = memoryPeer(() => {
      throw new Error("unexpected");
    });
    await peer.handleLine('{"jsonrpc":"2.0","id":2,"method":"x"}');
    expect(JSON.parse(written[0]!).error.code).toBe(-32603);
  });

  it("通知不回复", async () => {
    const { peer, written, notifications } = memoryPeer();
    expect(await peer.handleLine('{"jsonrpc":"2.0","method":"log","params":{"x":1}}')).toBe(true);
    expect(notifications).toEqual([{ method: "log", params: { x: 1 } }]);
    expect(written).toHaveLength(0);
  });

  it("响应交给 onResponse", async () => {
    const { peer, responses } = memoryPeer();
    expect(await peer.handleLine('{"jsonrpc":"2.0","id":9,"result":{"pong":true}}')).toBe(true);
    expect(responses).toEqual([{ id: 9, result: { pong: true } }]);
  });

  it("非法帧返回 false", async () => {
    const { peer } = memoryPeer();
    expect(await peer.handleLine("not json")).toBe(false);
    expect(await peer.handleLine('"scalar"')).toBe(false);
    expect(await peer.handleLine("[]")).toBe(false);
    expect(await peer.handleLine('{"method":"x"}')).toBe(false);
    expect(await peer.handleLine('{"jsonrpc":"2.0"}')).toBe(false);
  });
});

describe("错误帧细节", () => {
  it("replyError 可携带 data", async () => {
    const { peer, written } = memoryPeer();
    await peer.replyError(7, 1005, "会话不存在", { session: "s" });
    expect(JSON.parse(written[0]!)).toEqual({
      jsonrpc: "2.0",
      id: 7,
      error: { code: 1005, message: "会话不存在", data: { session: "s" } },
    });
  });

  it("写入抛出错误向上传递", async () => {
    const failure = new Error("broken pipe");
    const peer = new JsonRpcPeer(
      vi.fn(async () => {
        throw failure;
      }),
      { onRequest: () => null, onNotification: () => {} },
    );
    await expect(peer.notify("hello")).rejects.toThrow("broken pipe");
  });
});
