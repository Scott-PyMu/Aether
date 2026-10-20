/**
 * M7-03/ADR-017：会话暂停/恢复前端（锚点 `session-pause-toggle`、`paused-notice`）。
 *
 * 覆盖：
 * - 顶栏暂停/恢复入口（`data-session-id` / `data-status` / `data-disabled`）与调用
 *   （`session_pause` / `session_resume`）；
 * - 能力未声明（无 `pause_resume`）→ 置灰 + `pause_unsupported` 文案 + 结构化错误呈现；
 * - `paused` 门禁：发送与 `run_retry` 禁用、状态条提示与常驻语义声明（ADR-017 A-8）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import {
  PAUSE_SEMANTICS_NOTICE,
  PAUSE_UNSUPPORTED_HINT,
  PAUSED_STATUS_HINT,
  type MessagesPageResult,
  type PermissionMode,
  type RuntimeInfo,
  type SessionIpc,
  type SessionStatus,
  type SessionSummary,
} from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_1 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";

function runtime(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
  return {
    id: "claude-code",
    name: "Claude Code",
    kind: "acp",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: ["pause_resume"],
    enabled: true,
    status: "ready",
    status_reason: null,
    active_sessions: 0,
    ...overrides,
  };
}

function sessionSummary(
  status: SessionStatus,
  overrides: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "claude-code",
    workspace_id: null,
    parent_session_id: null,
    title: "暂停会话",
    status,
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    ...overrides,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION, max_seq: null, events: [], messages: [], complete: true };
}

function fakeIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    searchSessions: vi.fn(async () => []),
    renameSession: vi.fn(async (sessionId: string, title: string) => ({
      session_id: sessionId,
      title,
      updated_at: 0,
    })),
    deleteSession: vi.fn(async (sessionId: string) => ({ session_id: sessionId, deleted: true })),
    listSessions: vi.fn(async () => [sessionSummary("idle")]),
    createSession: vi.fn(async () => sessionSummary("idle")),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION, interrupted_run: null })),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(async () => ({ session_id: SESSION, status: "completed" as const })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
    ...overrides,
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

async function openSession(ipc: SessionIpc): Promise<void> {
  render(<SessionWorkbench store={newStore()} ipc={ipc} />);
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
}

describe("M7-03 暂停/恢复入口（session-pause-toggle）", () => {
  it("能力声明 + running：点击调用 session_pause；data-status/data-disabled 契约", async () => {
    const pauseSession = vi.fn(async (sessionId: string) => ({
      session_id: sessionId,
      status: "paused" as const,
      from: "running" as const,
    }));
    const ipc = fakeIpc({
      listSessions: async () => [sessionSummary("running")],
      pauseSession,
    });
    await openSession(ipc);

    const toggle = screen.getByTestId("session-pause-toggle");
    expect(toggle.getAttribute("data-session-id")).toBe(SESSION);
    expect(toggle.getAttribute("data-status")).toBe("running");
    expect(toggle.getAttribute("data-disabled")).toBe("false");
    expect(toggle.textContent).toContain("暂停");
    fireEvent.click(toggle);
    await waitFor(() => expect(pauseSession).toHaveBeenCalledWith(SESSION));
  });

  it("paused 会话：入口切为「恢复」并调用 session_resume；发送与重试禁用 + 门禁提示", async () => {
    const resumeSession = vi.fn(async (sessionId: string) => ({
      session_id: sessionId,
      status: "idle" as const,
      from: "paused" as const,
    }));
    const ipc = fakeIpc({
      // 首轮返回 paused；恢复后（refreshSessions）返回 idle。
      listSessions: vi
        .fn(async (): Promise<SessionSummary[]> => [])
        .mockResolvedValueOnce([sessionSummary("paused")]),
      resumeSession,
    });
    await openSession(ipc);

    const toggle = screen.getByTestId("session-pause-toggle");
    expect(toggle.getAttribute("data-status")).toBe("paused");
    expect(toggle.getAttribute("data-disabled")).toBe("false");
    expect(toggle.textContent).toContain("恢复");

    // paused 门禁：发送禁用（草稿保留）；门禁提示含状态行 + 常驻语义声明。
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(true);
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
    const notice = screen.getByTestId("paused-notice");
    expect(notice.textContent).toContain(PAUSED_STATUS_HINT);
    expect(notice.textContent).toContain(PAUSE_SEMANTICS_NOTICE);

    fireEvent.click(toggle);
    await waitFor(() => expect(resumeSession).toHaveBeenCalledWith(SESSION));
  });

  it("能力未声明（无 pause_resume）：置灰 + pause_unsupported tooltip；点击不发生命令", async () => {
    const pauseSession = vi.fn();
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({ capabilities: [] })],
      listSessions: async () => [sessionSummary("running")],
      pauseSession,
    });
    await openSession(ipc);

    const toggle = screen.getByTestId("session-pause-toggle");
    expect(toggle.getAttribute("data-disabled")).toBe("true");
    expect((toggle as HTMLButtonElement).disabled).toBe(true);
    expect(toggle.getAttribute("title")).toBe(PAUSE_UNSUPPORTED_HINT);
    fireEvent.click(toggle);
    expect(pauseSession).not.toHaveBeenCalled();
  });

  it("pause_unsupported 结构化错误呈现（workbench-error + data-code）", async () => {
    const ipc = fakeIpc({
      listSessions: async () => [sessionSummary("running")],
      pauseSession: vi.fn(async () => {
        throw {
          code: "pause_unsupported",
          message: "当前运行时不支持暂停/恢复（需原生会话恢复能力）：mock（未声明 pause_resume 能力）；会话状态未改变",
        };
      }),
    });
    await openSession(ipc);

    fireEvent.click(screen.getByTestId("session-pause-toggle"));
    const error = await screen.findByTestId("workbench-error");
    expect(error.getAttribute("data-code")).toBe("pause_unsupported");
    expect(error.textContent).toContain("不支持暂停/恢复");
  });

  it("无会话不渲染入口", async () => {
    const ipc = fakeIpc({ listSessions: async () => [] });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findAllByTestId("runtime-option");
    expect(screen.queryByTestId("session-pause-toggle")).toBeNull();
  });
});
