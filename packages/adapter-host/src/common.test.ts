import { describe, expect, it } from "vitest";

import { extractAdapter } from "./common";

describe("extractAdapter", () => {
  it("剥离 --adapter <id>，保留其余参数顺序", () => {
    expect(extractAdapter(["--launch-token=01ABC", "--adapter", "codex", "--workspace", "w"])).toEqual({
      adapter: "codex",
      rest: ["--launch-token=01ABC", "--workspace", "w"],
    });
  });

  it("支持 --adapter=<id>", () => {
    expect(extractAdapter(["--adapter=mock", "--stream-deltas", "4"])).toEqual({
      adapter: "mock",
      rest: ["--stream-deltas", "4"],
    });
  });

  it("缺少 --adapter 返回错误", () => {
    const parsed = extractAdapter(["--launch-token=01ABC"]);
    expect(parsed.adapter).toBeNull();
    expect(parsed.error).toContain("--adapter");
  });

  it("--adapter 缺少值返回错误", () => {
    const parsed = extractAdapter(["--adapter"]);
    expect(parsed.adapter).toBeNull();
    expect(parsed.error).toContain("--adapter");
  });
});
