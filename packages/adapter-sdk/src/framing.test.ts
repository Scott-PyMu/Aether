import { describe, expect, it } from "vitest";

import { encodeLine, FramingError, LineReader } from "./framing";
import { ARTIFACT_REF_LIMIT_BYTES, MAX_FRAME_BYTES } from "./protocol";

describe("帧编码（D6）", () => {
  it("追加 LF", () => {
    expect(encodeLine('{"a":1}')).toBe('{"a":1}\n');
  });

  it("拒绝内嵌 CR/LF（防帧注入）", () => {
    expect(() => encodeLine("a\nb")).toThrow(FramingError);
    expect(() => encodeLine("a\rb")).toThrow(FramingError);
    try {
      encodeLine("a\nb");
    } catch (error) {
      expect((error as FramingError).kind).toBe("invalid-outbound");
    }
  });

  it("拒绝超过 2MiB 的行", () => {
    const giant = "x".repeat(MAX_FRAME_BYTES + 1);
    expect(() => encodeLine(giant)).toThrowError(/2MiB|上限/);
  });

  it("拒绝 1–2MiB 声称 artifact_ref 的帧（契约违约）", () => {
    const pad = "a".repeat(ARTIFACT_REF_LIMIT_BYTES);
    const line = `{"type":"artifact_ref","params":{"pad":"${pad}"}}`;
    expect(Buffer.byteLength(line, "utf8")).toBeGreaterThanOrEqual(ARTIFACT_REF_LIMIT_BYTES);
    try {
      encodeLine(line);
      throw new Error("应当抛出契约违约错误");
    } catch (error) {
      expect((error as FramingError).kind).toBe("artifact-ref-contract");
    }
  });

  it("1–2MiB 非引用行允许（D6 正常解析）", () => {
    const pad = "y".repeat(ARTIFACT_REF_LIMIT_BYTES);
    const line = `{"method":"log","params":{"pad":"${pad}"}}`;
    expect(Buffer.byteLength(line, "utf8")).toBeGreaterThan(ARTIFACT_REF_LIMIT_BYTES);
    expect(() => encodeLine(line)).not.toThrow();
  });
});

describe("增量行读取", () => {
  it("分块/CRLF/空行/残行", () => {
    const reader = new LineReader();
    expect(reader.push('{"a":1}\n{"b"')).toEqual(['{"a":1}']);
    expect(reader.push(':2}\r\n\n   \n')).toEqual(['{"b":2}']);
    expect(reader.push('{"c":3}')).toEqual([]);
    expect(reader.flush()).toBe('{"c":3}');
    expect(reader.flush()).toBeUndefined();
  });

  it("字节块跨行 UTF-8", () => {
    const reader = new LineReader();
    const bytes = new TextEncoder().encode('{"t":"中文"}\n');
    const first = bytes.slice(0, 10);
    const second = bytes.slice(10);
    expect(reader.push(first)).toEqual([]);
    expect(reader.push(second)).toEqual(['{"t":"中文"}']);
  });

  it("非法 UTF-8 抛错", () => {
    const reader = new LineReader();
    expect(() => reader.push(new Uint8Array([0xff, 0xfe, 0xfd]))).toThrow(FramingError);
  });

  it("超限残行抛错", () => {
    const reader = new LineReader();
    expect(() => reader.push("x".repeat(MAX_FRAME_BYTES + 1))).toThrowError(/上限/);
  });

  it("越界 artifact_ref 残行抛契约违约错误", () => {
    const reader = new LineReader();
    const pad = "a".repeat(ARTIFACT_REF_LIMIT_BYTES);
    expect(() => reader.push(`{"type":"artifact_ref","params":{"pad":"${pad}"}}`)).toThrowError(
      /引用帧/,
    );
  });
});
