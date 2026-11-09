import { afterEach, describe, expect, it, vi } from "vitest";

import { client } from "@aether/protocol";

import { subagentIpc, type SubagentSpawnInput } from "./subagents";

const PARENT = "01JPARENT0000000000000000";
const CHILD = "01JCHILD00000000000000000";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("M10-02 subagentIpc 命令封装", () => {
  it("list/spawn/cancel/resolve/resultGet 映射到 typed client", async () => {
    const list = vi.spyOn(client, "subagentList").mockResolvedValue({ items: [] });
    const spawn = vi.spyOn(client, "subagentSpawn").mockResolvedValue({
      child_session_id: CHILD,
      status: "running",
      depth: 1,
      mode: "review",
      batch_id: "batch",
    });
    const cancel = vi.spyOn(client, "subagentCancel").mockResolvedValue({
      status: "cancelled",
    });
    const resolve = vi.spyOn(client, "subagentResolve").mockResolvedValue({
      decision: "accept",
      injected: true,
      context_version: 1,
      mode: "message_append",
    });
    const resultGet = vi.spyOn(client, "subagentResultGet").mockResolvedValue({
      child_session_id: CHILD,
      result_chunk: "chunk",
      offset: 0,
      next_offset: null,
      total_bytes: 5,
      truncated: false,
    });

    await expect(subagentIpc.list(PARENT)).resolves.toEqual({ items: [] });
    expect(list).toHaveBeenCalledWith({
      parent_session_id: PARENT,
      include_completed: true,
    });
    await subagentIpc.list(PARENT, false);
    expect(list).toHaveBeenLastCalledWith({
      parent_session_id: PARENT,
      include_completed: false,
    });

    const input: SubagentSpawnInput = {
      parent_session_id: PARENT,
      role: "review",
      purpose: "审查",
      template: "adversarial_review",
    };
    await expect(subagentIpc.spawn(input)).resolves.toMatchObject({
      child_session_id: CHILD,
    });
    expect(spawn).toHaveBeenCalledWith(input);

    await expect(subagentIpc.cancel(CHILD)).resolves.toEqual({
      status: "cancelled",
    });
    expect(cancel).toHaveBeenCalledWith({ child_session_id: CHILD });
    await subagentIpc.cancel(CHILD, "user");
    expect(cancel).toHaveBeenLastCalledWith({
      child_session_id: CHILD,
      reason: "user",
    });

    await expect(subagentIpc.resolve(CHILD, "accept")).resolves.toMatchObject({
      decision: "accept",
    });
    expect(resolve).toHaveBeenCalledWith({
      child_session_id: CHILD,
      decision: "accept",
    });
    await subagentIpc.resolve(CHILD, "reject", "证据不足");
    expect(resolve).toHaveBeenLastCalledWith({
      child_session_id: CHILD,
      decision: "reject",
      note: "证据不足",
    });

    await expect(subagentIpc.resultGet(PARENT, CHILD)).resolves.toMatchObject({
      result_chunk: "chunk",
    });
    expect(resultGet).toHaveBeenCalledWith({
      parent_session_id: PARENT,
      child_session_id: CHILD,
      offset: 0,
      limit_bytes: 16_384,
    });
    await subagentIpc.resultGet(PARENT, CHILD, 10, 20);
    expect(resultGet).toHaveBeenLastCalledWith({
      parent_session_id: PARENT,
      child_session_id: CHILD,
      offset: 10,
      limit_bytes: 20,
    });
  });
});
