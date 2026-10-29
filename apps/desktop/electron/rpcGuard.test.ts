/**
 * main 侧 RPC 防线单测（M9-10 DoD-2）。
 *
 * 覆盖：未知方法 / 2MiB 边界 / 并发 64 边界 / 不可序列化参数。
 */
import { describe, expect, it } from "vitest";

import {
  MAX_CONCURRENT_RPC,
  MAX_RPC_PAYLOAD_BYTES,
  guardRpcRequest,
  payloadSizeBytes,
} from "./rpcGuard";

const COMMANDS = new Set(["health", "session_list", "session_send"]);

function guard(method: string, params?: unknown, inFlight = 0) {
  return guardRpcRequest({ method, params, inFlight, commands: COMMANDS });
}

describe("rpcGuard", () => {
  it("命令清单内的方法放行", () => {
    expect(guard("health", {})).toEqual({ ok: true });
    expect(guard("session_send", { text: "hi" }, MAX_CONCURRENT_RPC - 1)).toEqual({
      ok: true,
    });
  });

  it("未知方法拒绝为 not_implemented（-32601）", () => {
    const decision = guard("definitely_not_a_command", {});
    expect(decision.ok).toBe(false);
    if (!decision.ok) {
      expect(decision.rejection.code).toBe(-32601);
      expect(decision.rejection.aetherCode).toBe("not_implemented");
    }
  });

  it("空方法名拒绝为 invalid_json（-32600）", () => {
    const decision = guard("", {});
    expect(decision.ok).toBe(false);
    if (!decision.ok) {
      expect(decision.rejection.code).toBe(-32600);
      expect(decision.rejection.aetherCode).toBe("invalid_json");
    }
  });

  it("2MiB 边界：恰好 2MiB 放行，超过 1 字节拒绝", () => {
    const prefix = '{"text":"';
    const suffix = '"}';
    const exact = "a".repeat(
      MAX_RPC_PAYLOAD_BYTES - Buffer.byteLength(prefix) - Buffer.byteLength(suffix),
    );
    expect(payloadSizeBytes({ text: exact })).toBe(MAX_RPC_PAYLOAD_BYTES);
    expect(guard("session_send", { text: exact })).toEqual({ ok: true });

    const over = guard("session_send", { text: `${exact}a` });
    expect(over.ok).toBe(false);
    if (!over.ok) {
      expect(over.rejection.code).toBe(-32003);
      expect(over.rejection.aetherCode).toBe("too_large");
    }
  });

  it("不可序列化参数按超限拒绝（不抛异常）", () => {
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(payloadSizeBytes(circular)).toBe(Number.POSITIVE_INFINITY);
    const decision = guard("session_send", circular);
    expect(decision.ok).toBe(false);
    if (!decision.ok) {
      expect(decision.rejection.aetherCode).toBe("too_large");
    }
  });

  it("并发边界：63 放行、64 拒绝（第 65 个起拒绝）", () => {
    expect(guard("health", {}, MAX_CONCURRENT_RPC - 1)).toEqual({ ok: true });
    const decision = guard("health", {}, MAX_CONCURRENT_RPC);
    expect(decision.ok).toBe(false);
    if (!decision.ok) {
      expect(decision.rejection.code).toBe(-32603);
      expect(decision.rejection.aetherCode).toBe("internal");
    }
  });

  it("undefined 参数计 0 字节", () => {
    expect(payloadSizeBytes(undefined)).toBe(0);
    expect(guard("health", undefined)).toEqual({ ok: true });
  });
});
