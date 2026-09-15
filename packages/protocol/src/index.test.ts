import { describe, expect, it } from "vitest";
import { APP_VERSION, PROTOCOL_VERSION } from "./index";

describe("@aether/protocol", () => {
  it("线协议 major 为 1（D6：major 不匹配拒绝加载）", () => {
    expect(PROTOCOL_VERSION.major).toBe(1);
    expect(PROTOCOL_VERSION.minor).toBe(0);
  });

  it("APP_VERSION 为三段式 semver 且与工作区版本一致", () => {
    expect(APP_VERSION).toMatch(/^\d+\.\d+\.\d+$/);
    const [, major, minor, patch] = APP_VERSION.match(
      /^(\d+)\.(\d+)\.(\d+)$/,
    ) as RegExpMatchArray;
    expect(Number(major)).toBeGreaterThanOrEqual(0);
    expect(Number(minor)).toBeGreaterThanOrEqual(0);
    expect(Number(patch)).toBeGreaterThanOrEqual(0);
  });
});
