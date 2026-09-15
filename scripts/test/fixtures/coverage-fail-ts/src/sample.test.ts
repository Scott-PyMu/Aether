import { describe, expect, it } from "vitest";
import { covered } from "./covered";

describe("覆盖率门禁夹具", () => {
  it("仅覆盖 covered.ts", () => {
    expect(covered(1)).toBe(2);
  });
});
