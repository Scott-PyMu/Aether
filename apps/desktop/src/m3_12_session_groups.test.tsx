/**
 * M3-12 前端集成 E2E：会话列表等待审批分组（UI-UX §2.2；ADR-011 决策 5 锚点契约）。
 *
 * 断言：`session-group` 的 `data-group` 序列 = 非空组固定组序子集；空组不渲染；
 * 等待态出现/消失时 `waiting_permission` 组的增删；`session-item` 不跨组错位；
 * 既有锚点（`session-list`/`session-item`/`data-session-id`/`data-active`）不重命名。
 */
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";

function summary(
  id: string,
  status: SessionStatus,
  updatedAt: number,
  title = id,
): SessionSummary {
  return {
    id,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title,
    status,
    model: null,
    created_at: 1,
    updated_at: updatedAt,
    closed_at: null,
  };
}

function emptyPage(sessionId: string): MessagesPageResult {
  return {
    session_id: sessionId,
    max_seq: null,
    events: [],
    messages: [],
    complete: true,
  };
}

const stores: EventStore[] = [];

function newStore(): EventStore {
  const store = new EventStore();
  stores.push(store);
  return store;
}

afterEach(() => {
  cleanup();
  for (const store of stores.splice(0)) {
    store.dispose();
  }
  vi.restoreAllMocks();
});

describe("SessionWorkbench 会话分组（M3-12 / ADR-011）", () => {
  it("固定组序 + 空组隐藏 + 等待审批组出现/消失", async () => {
    const runtimes: RuntimeInfo[] = [
      {
        id: "mock",
        name: "Mock",
        kind: "mock",
        version: "0.1.0",
        protocol: "1.0",
        capabilities: [],
        enabled: true,
        status: "ready",
        status_reason: null,
      },
    ];
    // 后备数据源：等待态可通过测试侧改写后触发 refresh（发送 ack 路径）。
    let sessions: SessionSummary[] = [
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1A", "running", 40, "运行会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1B", "waiting_permission", 30, "等待会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1C", "failed", 20, "失败会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1D", "idle", 10, "空闲会话"),
    ];
    const ipc: SessionIpc = {
      listRuntimes: vi.fn(async () => runtimes),
      listSessions: vi.fn(async () => sessions),
      createSession: vi.fn(async (input) =>
        summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1E", "idle", 50, input.title),
      ),
      sendMessage: vi.fn(async (input) => ({
        session_id: input.session_id,
        message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
        run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
        queued: false,
        duplicate: false,
      })),
      interruptSession: vi.fn(async (sessionId) => ({
        session_id: sessionId,
        interrupted_run: null,
      })),
      disposeSession: vi.fn(async (sessionId) => ({
        session_id: sessionId,
        status: "completed" as SessionStatus,
      })),
      messagesPage: vi.fn(async (input) => emptyPage(input.session_id)),
      retryRun: vi.fn(async () => ({
        session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
        run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
        input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
        queued: false,
      })),
    };
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);

    // 固定组序 = 非空组的白名单子集（运行中 → 等待审批 → 失败 → 其他）。
    await waitFor(() => {
      expect(screen.getAllByTestId("session-group")).toHaveLength(4);
    });
    const groups = () => screen.getAllByTestId("session-group");
    expect(groups().map((node) => node.getAttribute("data-group"))).toEqual([
      "running",
      "waiting_permission",
      "failed",
      "other",
    ]);
    const groupOf = (groupId: string) =>
      groups().find((node) => node.getAttribute("data-group") === groupId);
    expect(
      groupOf("running")
        ?.querySelectorAll('[data-testid="session-item"]')
        .length,
    ).toBe(1);
    expect(
      groupOf("waiting_permission")
        ?.querySelectorAll('[data-testid="session-item"]')
        .length,
    ).toBe(1);
    expect(
      groupOf("failed")
        ?.querySelectorAll('[data-testid="session-item"]')
        .length,
    ).toBe(1);
    expect(
      groupOf("other")
        ?.querySelectorAll('[data-testid="session-item"]')
        .length,
    ).toBe(1);
    // 既有锚点不重命名：每个会话项仍在 `session-list` 容器内且带 data-session-id。
    for (const item of screen.getAllByTestId("session-item")) {
      expect(item.getAttribute("data-session-id")).toMatch(/^01J/);
      expect(screen.getByTestId("session-list").contains(item)).toBe(true);
    }

    // 等待态消失：会话 B 决议后回 running（触发 sender ack → refreshSessions 重取）。
    sessions = [
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1A", "running", 40, "运行会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1B", "running", 31, "等待会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1C", "failed", 20, "失败会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1D", "idle", 10, "空闲会话"),
    ];
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "触发刷新" },
    });
    await act(async () => {
      fireEvent.click(screen.getByTestId("composer-send"));
    });
    await waitFor(() => {
      expect(
        groups().map((node) => node.getAttribute("data-group")),
      ).toEqual(["running", "failed", "other"]);
    });
    expect(groupOf("waiting_permission")).toBeUndefined();
    // 运行中组内按 updated_at 倒序：B(31) 在 A(40) 之后。
    expect(
      [...(groupOf("running")?.querySelectorAll('[data-testid="session-item"]') ?? [])].map(
        (item) => item.getAttribute("data-session-id"),
      ),
    ).toEqual([
      "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
      "01J8ZQ5R0N7W9Y8X6V4T2S0K1B",
    ]);

    // 等待态再次出现：会话 A 进入 waiting_permission。
    sessions = [
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1A", "waiting_permission", 41, "运行会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1B", "running", 31, "等待会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1C", "failed", 20, "失败会话"),
      summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1D", "idle", 10, "空闲会话"),
    ];
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "再次刷新" },
    });
    await act(async () => {
      fireEvent.click(screen.getByTestId("composer-send"));
    });
    await waitFor(() => {
      expect(
        groups().map((node) => node.getAttribute("data-group")),
      ).toEqual(["running", "waiting_permission", "failed", "other"]);
    });
    expect(
      groupOf("waiting_permission")?.querySelectorAll('[data-testid="session-item"]')
        .length,
    ).toBe(1);
  });

  it("空组隐藏：仅其他组时不渲染运行中/等待审批/失败组", async () => {
    const ipc: SessionIpc = {
      listRuntimes: vi.fn(async () => []),
      listSessions: vi.fn(async () => [
        summary("01J8ZQ5R0N7W9Y8X6V4T2S0K1D", "idle", 10, "空闲会话"),
      ]),
      createSession: vi.fn(),
      sendMessage: vi.fn(),
      interruptSession: vi.fn(),
      disposeSession: vi.fn(),
      messagesPage: vi.fn(async (input) => emptyPage(input.session_id)),
      retryRun: vi.fn(),
    } as unknown as SessionIpc;
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);

    await waitFor(() => {
      expect(screen.getAllByTestId("session-group")).toHaveLength(1);
    });
    expect(screen.getByTestId("session-group").getAttribute("data-group")).toBe("other");
    for (const groupId of ["running", "waiting_permission", "failed"]) {
      expect(
        screen
          .queryAllByTestId("session-group")
          .some((node) => node.getAttribute("data-group") === groupId),
      ).toBe(false);
    }
  });
});
