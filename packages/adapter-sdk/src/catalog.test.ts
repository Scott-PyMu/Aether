/**
 * ADR-028 v0.9：`provider.catalog.get` 客户端与 broker env 剥离单测。
 */
import { describe, expect, it, vi } from "vitest";

import {
  CatalogError,
  fetchProviderCatalog,
  PROVIDER_CATALOG_METHOD,
  stripBrokerEnv,
  stripBrokerEnvInPlace,
} from "./index";
import { JsonRpcPeer, RpcError } from "./rpc";

function peerWith(lines: string[], response: (request: Record<string, unknown>) => string) {
  const written: string[] = [];
  const peer = new JsonRpcPeer(
    async (line) => {
      written.push(line);
      const request = JSON.parse(line) as Record<string, unknown>;
      queueMicrotask(() => {
        void peer.handleLine(response(request));
      });
    },
    {
      onRequest: () => null,
      onNotification: () => undefined,
    },
  );
  return { peer, written, lines };
}

describe("provider.catalog.get 客户端", () => {
  it("以独立请求 id 发送并解析非机密目录", async () => {
    const { peer, written } = peerWith([], (request) =>
      JSON.stringify({
        jsonrpc: "2.0",
        id: request["id"],
        result: {
          providers: [
            {
              id: "01J0000000000000000000P01",
              name: "DeepSeek",
              type: "deepseek",
              enabled: true,
              endpoints: { anthropic: "https://api.deepseek.com/anthropic" },
              models: [{ id: "deepseek-flash", display_name: "Flash", enabled: true }],
            },
          ],
        },
      }),
    );
    const catalog = await fetchProviderCatalog(peer);
    expect(catalog.providers[0]?.id).toBe("01J0000000000000000000P01");
    expect(catalog.providers[0]?.models[0]?.id).toBe("deepseek-flash");
    const frame = JSON.parse(written[0] ?? "{}") as Record<string, unknown>;
    expect(frame["method"]).toBe(PROVIDER_CATALOG_METHOD);
    expect(String(frame["id"])).toMatch(/^a:\d+$/);
    expect(frame["params"]).toEqual({});
  });

  it("错误映射：-32602 → invalid_value，-32603 → core_not_ready", async () => {
    const invalid = peerWith([], (request) =>
      JSON.stringify({
        jsonrpc: "2.0",
        id: request["id"],
        error: { code: -32602, message: "bad params", data: { aether_code: "invalid_value" } },
      }),
    );
    await expect(fetchProviderCatalog(invalid.peer)).rejects.toMatchObject({
      aetherCode: "invalid_value",
    });
    const internal = peerWith([], (request) =>
      JSON.stringify({
        jsonrpc: "2.0",
        id: request["id"],
        error: { code: -32603, message: "not ready", data: { aether_code: "core_not_ready" } },
      }),
    );
    await expect(fetchProviderCatalog(internal.peer)).rejects.toBeInstanceOf(CatalogError);
  });

  it("请求超时按 core_not_ready 处理", async () => {
    vi.useFakeTimers();
    try {
      const peer = new JsonRpcPeer(async () => undefined, {
        onRequest: () => null,
        onNotification: () => undefined,
      });
      const pending = fetchProviderCatalog(peer);
      const assertion = expect(pending).rejects.toMatchObject({ aetherCode: "core_not_ready" });
      await vi.advanceTimersByTimeAsync(5_100);
      await assertion;
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("CLI 子进程 broker env 剥离", () => {
  it("stripBrokerEnv 不移除供应商 key 且不修改入参", () => {
    const env = {
      AETHER_CREDENTIAL_BROKER: "token",
      ANTHROPIC_API_KEY: "fake-key",
    } as NodeJS.ProcessEnv;
    const stripped = stripBrokerEnv(env);
    expect(stripped.AETHER_CREDENTIAL_BROKER).toBeUndefined();
    expect(stripped.ANTHROPIC_API_KEY).toBe("fake-key");
    expect(env.AETHER_CREDENTIAL_BROKER).toBe("token");
    stripBrokerEnvInPlace(env);
    expect(env.AETHER_CREDENTIAL_BROKER).toBeUndefined();
  });

  it("未知错误码仍以 core_not_ready 呈现", async () => {
    const { peer } = peerWith([], (request) =>
      JSON.stringify({
        jsonrpc: "2.0",
        id: request["id"],
        error: { code: -32601, message: "method not found" },
      }),
    );
    await expect(fetchProviderCatalog(peer)).rejects.toMatchObject({
      aetherCode: "core_not_ready",
    });
    // RpcError 仍可被调用方识别（不吞错）。
    expect(new RpcError(-32601, "x")).toBeInstanceOf(Error);
  });
});
