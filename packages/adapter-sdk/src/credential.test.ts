import { randomUUID } from "node:crypto";
import { unlinkSync } from "node:fs";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { CredentialClient } from "./credential";

function endpointPath(): string {
  return process.platform === "win32"
    ? `\\\\.\\pipe\\aether-credential-test-${randomUUID()}`
    : join(tmpdir(), `aether-credential-test-${randomUUID()}.sock`);
}

async function startBroker(
  path: string,
  response: unknown,
  onRequest?: (request: Record<string, unknown>) => void,
): Promise<Server> {
  const server = createServer((socket) => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline < 0) return;
      const request = JSON.parse(buffer.slice(0, newline)) as Record<string, unknown>;
      onRequest?.(request);
      socket.end(`${JSON.stringify(response)}\n`);
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(path, () => resolve());
  });
  return server;
}

async function closeServer(server: Server, path: string): Promise<void> {
  await new Promise<void>((resolve) => server.close(() => resolve()));
  if (process.platform !== "win32") {
    try {
      unlinkSync(path);
    } catch {
      // 已清理/不存在；测试不关心。
    }
  }
}

describe("CredentialClient (ADR-028 / M7-10)", () => {
  it("按 provider + protocol 取单供应商凭据；不缓存、不在客户端落盘", async () => {
    const path = endpointPath();
    const requests: Array<Record<string, unknown>> = [];
    const server = await startBroker(
      path,
      {
        ok: true,
        base_url: "https://api.deepseek.com/anthropic",
        api_key: "test-broker-key",
        auth_env: "ANTHROPIC_API_KEY",
      },
      (request) => requests.push(request),
    );
    try {
      const client = CredentialClient.fromEnv({
        AETHER_CREDENTIAL_BROKER: JSON.stringify({ endpoint: path, token: "test-token" }),
      } as NodeJS.ProcessEnv);
      expect(client).not.toBeNull();
      const credential = await client!.get(
        "01J0000000000000000000M710",
        "anthropic",
        "claude-code",
      );
      expect(credential).toEqual({
        base_url: "https://api.deepseek.com/anthropic",
        api_key: "test-broker-key",
        auth_env: "ANTHROPIC_API_KEY",
      });
      expect(requests).toEqual([
        {
          token: "test-token",
          provider_id: "01J0000000000000000000M710",
          protocol: "anthropic",
          runtime_id: "claude-code",
        },
      ]);
    } finally {
      await closeServer(server, path);
    }
  });

  it("broker 错误响应 → 抛错且不泄露 key", async () => {
    const path = endpointPath();
    const server = await startBroker(path, {
      ok: false,
      error: "供应商已停用",
    });
    try {
      const client = CredentialClient.fromEnv({
        AETHER_CREDENTIAL_BROKER: JSON.stringify({ endpoint: path, token: "test-token" }),
      } as NodeJS.ProcessEnv);
      await expect(client!.get("provider", "anthropic")).rejects.toThrow("供应商已停用");
    } finally {
      await closeServer(server, path);
    }
  });

  it("未配置/非法 broker env → null（旧单供应商路径）", () => {
    expect(CredentialClient.fromEnv({} as NodeJS.ProcessEnv)).toBeNull();
    expect(
      CredentialClient.fromEnv({
        AETHER_CREDENTIAL_BROKER: "not-json",
      } as NodeJS.ProcessEnv),
    ).toBeNull();
    expect(
      CredentialClient.fromEnv({
        AETHER_CREDENTIAL_BROKER: JSON.stringify({ endpoint: "", token: "" }),
      } as NodeJS.ProcessEnv),
    ).toBeNull();
  });
});
