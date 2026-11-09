import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { SubagentPanel } from "./SubagentPanel";
import type { SubagentIpc, SubagentNode } from "./subagents";

const PARENT = "01JPARENT0000000000000000";
const CHILD = "01JCHILD00000000000000000";

function node(overrides: Partial<SubagentNode> = {}): SubagentNode {
  return {
    child_session_id: CHILD,
    parent_session_id: PARENT,
    title: "子 Agent · review · 审查",
    role: "review",
    mode: "review",
    depth: 1,
    status: "completed",
    verdict: "needs_changes",
    result_summary: "发现 1 个问题",
    summary_truncated: false,
    result_ref: `subagent:${CHILD}`,
    injection_state: "pending",
    delivery_mode: "message_append",
    has_more_details: true,
    created_at: 1,
    ...overrides,
  };
}

function fakeIpc(initial: SubagentNode[]): SubagentIpc {
  let nodes = initial;
  return {
    list: vi.fn(async () => ({ items: nodes })),
    spawn: vi.fn(async () => ({
      child_session_id: CHILD,
      status: "running",
      depth: 1,
      mode: "review",
      batch_id: "batch",
    })),
    cancel: vi.fn(async () => ({ status: "cancelled" })),
    resolve: vi.fn(async (_child, decision) => {
      nodes = nodes.map((item) => ({
        ...item,
        injection_state:
          decision === "accept"
            ? "injected"
            : decision === "reject"
              ? "rejected"
              : "superseded",
      }));
      return {
        decision,
        injected: decision === "accept",
        injection_id: "inj-1",
        context_version: 1,
        mode: "message_append",
      };
    }),
    resultGet: vi.fn(async () => ({
      child_session_id: CHILD,
      result_chunk: '{"verdict":"needs_changes","summary":"发现 1 个问题"}',
      offset: 0,
      next_offset: null,
      total_bytes: 48,
      truncated: false,
    })),
  };
}

describe("M10-02 子会话树 / 结果卡（UI-03）", () => {
  it("展示父子关系、状态、结果摘要与处置动作", async () => {
    const ipc = fakeIpc([node()]);
    const store = new EventStore();
    render(
      <SubagentPanel
        parentSessionId={PARENT}
        store={store}
        ipc={ipc}
        onOpenChild={() => undefined}
      />,
    );

    expect(
      (await screen.findByTestId("subagent-tree")).getAttribute(
        "data-parent-session-id",
      ),
    ).toBe(PARENT);
    const item = await screen.findByTestId("subagent-node");
    expect(item.getAttribute("data-depth")).toBe("1");
    expect(item.getAttribute("data-status")).toBe("completed");
    expect(item.getAttribute("data-verdict")).toBe("needs_changes");
    expect(item.getAttribute("data-injection-state")).toBe("pending");
    expect(screen.getByTestId("subagent-result").textContent).toContain("发现 1 个问题");

    const actions = screen.getAllByTestId("subagent-result-action");
    expect(actions.map((action) => action.getAttribute("data-action"))).toEqual(
      expect.arrayContaining(["accept", "reject", "rerun"]),
    );
    const accept = actions.find((action) => action.getAttribute("data-action") === "accept");
    expect((accept as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(accept as HTMLElement);
    await waitFor(() => expect(ipc.resolve).toHaveBeenCalledWith(CHILD, "accept"));
    await waitFor(() =>
      expect(
        screen.getByTestId("subagent-node").getAttribute("data-injection-state"),
      ).toBe("injected"),
    );

    fireEvent.click(
      screen
        .getAllByTestId("subagent-result-action")
        .find((action) => action.getAttribute("data-action") === "details") as HTMLElement,
    );
    expect((await screen.findByText(/needs_changes/)).textContent).toContain("needs_changes");
  });

  it("pending 之外不渲染可执行 accept/reject（终态/已注入）", async () => {
    const ipc = fakeIpc([
      node({ injection_state: "injected", result_summary: "已完成" }),
    ]);
    const store = new EventStore();
    render(
      <SubagentPanel parentSessionId={PARENT} store={store} ipc={ipc} />,
    );
    const item = await screen.findByTestId("subagent-node");
    expect(item.getAttribute("data-injection-state")).toBe("injected");
    const accept = screen
      .getAllByTestId("subagent-result-action")
      .find((action) => action.getAttribute("data-action") === "accept");
    expect((accept as HTMLButtonElement).disabled).toBe(true);
  });
});
