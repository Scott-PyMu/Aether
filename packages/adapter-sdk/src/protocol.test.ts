import { describe, expect, it } from "vitest";

import {
  ARTIFACT_REF_LIMIT_BYTES,
  ARTIFACT_REF_TYPE,
  ERROR_CODES,
  HANDSHAKE_TIMEOUT_MS,
  INVALID_FRAME_UNHEALTHY_THRESHOLD,
  isRpcMethod,
  MAX_FRAME_BYTES,
  METHOD_TIMEOUTS_MS,
  PROTOCOL_MAJOR,
  PROTOCOL_MINOR,
  PROTOCOL_VERSION,
  protocolMajor,
  RPC_METHODS,
  validateProtocol,
} from "./protocol";

describe("D6 协议常量", () => {
  it("协议版本为 1.0（JSON-RPC 2.0 / JSON-Lines）", () => {
    expect(PROTOCOL_MAJOR).toBe(1);
    expect(PROTOCOL_MINOR).toBe(0);
    expect(PROTOCOL_VERSION).toBe("1.0");
  });

  it("握手/健康/帧阈值与设计文档一致", () => {
    expect(HANDSHAKE_TIMEOUT_MS).toBe(10_000);
    expect(INVALID_FRAME_UNHEALTHY_THRESHOLD).toBe(20);
    expect(MAX_FRAME_BYTES).toBe(2 * 1024 * 1024);
    expect(ARTIFACT_REF_LIMIT_BYTES).toBe(1024 * 1024);
    expect(ARTIFACT_REF_TYPE).toBe("artifact_ref");
  });

  it("方法表与 D6 超时表逐项一致", () => {
    expect(METHOD_TIMEOUTS_MS).toEqual({
      initialize: 10_000,
      "session.create": 30_000,
      "session.send": 30_000,
      "session.interrupt": 5_000,
      "session.dispose": 15_000,
      "tools.list": 10_000,
      "permission.resolve": 5_000,
      "health.ping": 5_000,
      shutdown: 5_000,
    });
    expect(RPC_METHODS).toHaveLength(9);
    expect(isRpcMethod("session.send")).toBe(true);
    expect(isRpcMethod("session.listen")).toBe(false);
    expect(isRpcMethod("toString")).toBe(false);
  });

  it("错误码覆盖标准码 + 应用码 1001–1005", () => {
    expect(ERROR_CODES).toEqual({
      PARSE_ERROR: -32700,
      INVALID_REQUEST: -32600,
      METHOD_NOT_FOUND: -32601,
      INVALID_PARAMS: -32602,
      INTERNAL_ERROR: -32603,
      ADAPTER_CRASHED: 1001,
      REQUEST_TIMEOUT: 1002,
      VERSION_MISMATCH: 1003,
      CAPABILITY_MISSING: 1004,
      SESSION_NOT_FOUND: 1005,
    });
  });

  it("major 校验：同 major 兼容、异 major 拒绝、非法串拒绝", () => {
    expect(protocolMajor("1.0")).toBe(1);
    expect(protocolMajor("1.99")).toBe(1);
    expect(protocolMajor("2.0")).toBe(2);
    expect(protocolMajor("abc")).toBeUndefined();
    expect(protocolMajor("")).toBeUndefined();
    expect(validateProtocol("1.0")).toBe(true);
    expect(validateProtocol("2.0")).toBe(false);
    expect(validateProtocol("x")).toBe(false);
  });
});
