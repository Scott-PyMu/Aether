/**
 * M7-07/ADR-025：会话搜索 / 改名 / 删除前端（锚点 `session-search` /
 * `session-search-result` / `session-rename*` / `session-delete*`）。
 *
 * 覆盖：
 * - 搜索框防抖调用 `session_search`，结果展示 snippet + 命中来源；Ctrl+K 聚焦；
 * - 改名入口 → 输入 → 保存（调用 `session_rename`，刷新列表）；
 * - 删除入口 → 二次确认（不可恢复提示）→ 调用 `session_delete`；busy 错误呈现；
 * - 空查询回到常规分组列表。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import {
  type MessagesPageResult,
  type PermissionMode,
  type RuntimeInfo,
  type SessionIpc,
  type SessionSearchHit,
  type SessionStatus,
  type SessionSummary,
} from "./session";

const SESSION_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";

function runtime(): RuntimeInfo {
  return {
    id: "mock",
    name: "Mock",
    kind: "mock",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: [],
    enabled: true,
    status: "ready",
    status_reason: null,
    active_sessions: 0,
  };
}

function summary(
  id: string,
  title: string,
  status: SessionStatus = "idle",
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
    updated_at: 1,
    closed_at: null,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION_A, max_seq: null, events: [], messages: [], complete: true };
}

function hit(session: SessionSummary, snippet: string, source: "title" | "message"): SessionSearchHit {
  return { ...session, snippet, source };
}

function fakeIpc(sessions: SessionSummary[]) {
  const list = [...sessions];
  const ipc: SessionIpc = {
    listRuntimes: vi.fn(async () => [runtime()]),
    listSessions: vi.fn(async () => [...list]),
    searchSessions: vi.fn(async ({ query }: { query: string }) => {
      const needle = query.trim();
      if (!needle) return [];
      return list
        .filter((session) => session.title.includes(needle))
        .map((session) => hit(session, `[${needle}] …`, "title"));
    }),
    renameSession: vi.fn(async (sessionId: string, title: string) => {
      const target = list.find((session) => session.id === sessionId);
      if (target) target.title = title;
      return { session_id: sessionId, title, updated_at: 2 };
    }),
    deleteSession: vi.fn(async (sessionId: string) => {
      const index = list.findIndex((session) => session.id === sessionId);
      if (index >= 0) list.splice(index, 1);
      return { session_id: sessionId, deleted: true };
    }),
    createSession: vi.fn(async () => list[0] ?? summary(SESSION_A, "新会话")),
    sendMessage: vi.fn(),
    interruptSession: vi.fn(),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
  };
  return ipc;
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

describe("M7-07 会话检索与管理", () => {
  it("搜索防抖调用 session_search，结果展示 snippet/source；Ctrl+K 聚焦", async () => {
    const ipc = fakeIpc([summary(SESSION_A, "alpha 会话"), summary(SESSION_B, "beta 会话")]);
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("session-list");

    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const search = screen.getByTestId("session-search");
    expect(document.activeElement).toBe(search);

    fireEvent.change(search, { target: { value: "alpha" } });
    const results = await screen.findAllByTestId("session-search-result");
    await waitFor(() => expect(ipc.searchSessions).toHaveBeenCalled());
    expect(results).toHaveLength(1);
    const first = results[0];
    expect(first).toBeDefined();
    expect(first?.getAttribute("data-session-id")).toBe(SESSION_A);
    expect(first?.getAttribute("data-source")).toBe("title");
    expect(first?.textContent).toContain("[alpha]");
    expect(first?.textContent).toContain("标题命中");
  });

  it("改名入口 → 保存调用 session_rename 并刷新标题", async () => {
    const ipc = fakeIpc([summary(SESSION_A, "旧标题")]);
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("session-item");

    fireEvent.click(screen.getByTestId("session-rename"));
    const input = screen.getByTestId("session-rename-input");
    fireEvent.change(input, { target: { value: "新标题" } });
    fireEvent.click(screen.getByTestId("session-rename-save"));

    await waitFor(() =>
      expect(ipc.renameSession).toHaveBeenCalledWith(SESSION_A, "新标题"),
    );
    await waitFor(() =>
      expect(screen.getByTestId("session-item").textContent).toContain("新标题"),
    );
  });

  it("删除入口需二次确认（不可恢复提示），确认后调用 session_delete", async () => {
    const ipc = fakeIpc([summary(SESSION_A, "待删除")]);
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("session-item");

    fireEvent.click(screen.getByTestId("session-delete"));
    const box = screen.getByTestId("session-delete-confirm-box");
    expect(box.textContent).toContain("不可恢复");
    fireEvent.click(screen.getByTestId("session-delete-confirm"));

    await waitFor(() => expect(ipc.deleteSession).toHaveBeenCalledWith(SESSION_A));
    await waitFor(() => expect(screen.queryByTestId("session-item")).toBeNull());
  });

  it("删除失败（session_busy）呈现结构化错误且不删除列表项", async () => {
    const ipc = fakeIpc([summary(SESSION_A, "在途会话")]);
    ipc.deleteSession = vi.fn(async () => {
      throw { code: "invalid_value", message: "会话有在途 run（session_busy）" };
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("session-item");

    fireEvent.click(screen.getByTestId("session-delete"));
    fireEvent.click(screen.getByTestId("session-delete-confirm"));

    const error = await screen.findByTestId("session-manage-error");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("session_busy");
    expect(screen.getByTestId("session-item")).toBeTruthy();
  });

  it("清除搜索后回到常规分组列表", async () => {
    const ipc = fakeIpc([summary(SESSION_A, "alpha 会话")]);
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("session-list");

    fireEvent.change(screen.getByTestId("session-search"), { target: { value: "alpha" } });
    await screen.findAllByTestId("session-search-result");
    fireEvent.click(screen.getByTestId("session-search-clear"));

    await waitFor(() => expect(screen.queryByTestId("session-search-results")).toBeNull());
    expect(screen.getByTestId("session-list")).toBeTruthy();
  });
});
