import { beforeEach, describe, expect, it } from "vitest";

import { isUlid, resetUlidState, ulid } from "./ulid";

describe("ULID", () => {
  beforeEach(() => {
    resetUlidState();
  });

  it("生成 26 位 Crockford Base32", () => {
    const value = ulid(1_700_000_000_000);
    expect(value).toHaveLength(26);
    expect(isUlid(value)).toBe(true);
    expect(isUlid(`${value.slice(0, 25)}!`)).toBe(false);
    expect(isUlid("TOO-SHORT")).toBe(false);
  });

  it("同一毫秒内单调递增", () => {
    const first = ulid(1_700_000_000_000);
    const second = ulid(1_700_000_000_000);
    const third = ulid(1_700_000_000_000);
    expect(second > first).toBe(true);
    expect(third > second).toBe(true);
  });

  it("跨时间戳可排序", () => {
    const earlier = ulid(1_700_000_000_000);
    const later = ulid(1_700_000_000_001);
    expect(later > earlier).toBe(true);
  });

  it("1000 次生成无重复", () => {
    const seen = new Set<string>();
    for (let index = 0; index < 1000; index += 1) {
      seen.add(ulid(1_700_000_000_000 + Math.floor(index / 10)));
    }
    expect(seen.size).toBe(1000);
  });
});
