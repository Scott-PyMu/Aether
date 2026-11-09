import { describe, expect, it, vi } from "vitest";

import {
  parseSubagentContext,
  SUBAGENT_SPAWN_TOOL,
  SubagentClient,
} from "./subagent";

describe("M10-02 subagent SDK client", () => {
  it("spawn/cancel/status/result.get use registered methods and timeouts", async () => {
    const request = vi.fn(async (method: string, params: unknown) => ({
      method,
      params,
    }));
    const client = new SubagentClient({ request } as never);
    await client.spawn({
      parent_session_id: "p",
      role: "explore",
      purpose: "x",
    });
    expect(request).toHaveBeenLastCalledWith(
      "subagent.spawn",
      expect.objectContaining({ parent_session_id: "p" }),
      30_000,
    );
    await client.cancel("c", "stop");
    expect(request).toHaveBeenLastCalledWith(
      "subagent.cancel",
      { child_session_id: "c", reason: "stop" },
      5_000,
    );
    await client.status("c");
    expect(request).toHaveBeenLastCalledWith(
      "subagent.status",
      { child_session_id: "c" },
      10_000,
    );
    await client.resultGet("p", "c", 0, 16_384);
    expect(request).toHaveBeenLastCalledWith(
      "subagent.result.get",
      {
        parent_session_id: "p",
        child_session_id: "c",
        offset: 0,
        limit_bytes: 16_384,
      },
      10_000,
    );
  });

  it("exposes a subagent_spawn tool definition", () => {
    expect(SUBAGENT_SPAWN_TOOL.name).toBe("aether_subagent_spawn");
    expect(SUBAGENT_SPAWN_TOOL.input_schema.required).toEqual([
      "parent_session_id",
      "role",
      "purpose",
    ]);
  });

  it("parses/validates subagent_context", () => {
    expect(parseSubagentContext(undefined)).toBeUndefined();
    expect(
      parseSubagentContext({
        version: 2,
        block: "<aether:subagent-results/>",
        mode: "message_append",
      }),
    ).toEqual({
      version: 2,
      block: "<aether:subagent-results/>",
      mode: "message_append",
    });
    expect(() =>
      parseSubagentContext({ version: 0, block: "x" }),
    ).toThrow();
    expect(() =>
      parseSubagentContext({ version: 1, block: "x", mode: "replace" }),
    ).toThrow();
  });
});
