/**
 * M7-06/ADR-024：引用注入输入区呈现（前端）。
 *
 * - 锚点 `file-reference`（常驻容器 + `data-count`）；chip 携带 `data-path`/`data-kind`；
 * - `@` 入口：空输入框按 `@` 打开文件选择器并添加引用；非空不劫持；
 * - 随消息发送：`session_send` 携带 `references`（≤32；空数组省略字段）；成功后清空；
 * - 移除 chip 后再次发送不携带。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ArtifactsIpc } from "./artifacts";
import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_1 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";

function runtime(): RuntimeInfo {
  return {
    id: "claude-code",
    name: "Claude Code",
    kind: "acp",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: [],
    enabled: true,
    status: "ready",
    status_reason: null,
  };
}

function sessionSummary(status: SessionStatus, overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "claude-code",
    workspace_id: null,
    parent_session_id: null,
    title: "工作台会话",
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
    createSession: vi.fn(async (input) => sessionSummary("idle", { title: input.title })),
    sendMessage: vi.fn(async (input) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async (sessionId) => ({ session_id: sessionId, interrupted_run: null })),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(async () => ({
      session_id: SESSION,
      run_id: RUN_1,
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
    ...overrides,
  };
}

function fakeArtifactsIpc(pickPath: string | null): ArtifactsIpc & {
  pick: ReturnType<typeof vi.fn>;
} {
  return {
    pick: vi.fn(async () => pickPath),
    list: vi.fn(async () => []),
    add: vi.fn(async () => {
      throw new Error("unused");
    }),
    remove: vi.fn(async () => false),
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

async function openSession(ipc: SessionIpc, artifacts: ArtifactsIpc): Promise<void> {
  render(
    <SessionWorkbench
      store={newStore()}
      ipc={ipc}
      artifactsIpc={artifacts}
    />,
  );
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
}

describe("M7-06 引用注入输入区（ADR-024 A-3 / UI-UX §7.3 file-reference）", () => {
  it("空输入框按 @ 触发选择器并添加 chip（data-path/data-kind）；发送携带 references；成功后清空", async () => {
    const sendMessage = vi.fn(async (input: Parameters<SessionIpc["sendMessage"]>[0]) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    }));
    const ipc = fakeIpc({ sendMessage });
    const artifacts = fakeArtifactsIpc("C:\\ws\\note.txt");
    await openSession(ipc, artifacts);

    // 常驻容器（空态提示）。
    const container = screen.getByTestId("file-reference");
    expect(container.getAttribute("data-count")).toBe("0");

    fireEvent.keyDown(screen.getByTestId("composer-input"), { key: "@" });
    await screen.findByTestId("file-reference-chip", {}, { timeout: 5000 });
    expect(artifacts.pick).toHaveBeenCalledWith("file");
    const chip = screen.getByTestId("file-reference-chip");
    expect(chip.getAttribute("data-path")).toBe("C:\\ws\\note.txt");
    expect(chip.getAttribute("data-kind")).toBe("file");
    expect(screen.getByTestId("file-reference").getAttribute("data-count")).toBe("1");

    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "读一下引用" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1), { timeout: 5000 });
    const input = sendMessage.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(input.text).toBe("读一下引用");
    expect(input.references).toEqual([{ path: "C:\\ws\\note.txt", kind: "file" }]);

    // 发送成功后清空；下一次发送不带 references 字段。
    await waitFor(() => {
      expect(screen.getByTestId("file-reference").getAttribute("data-count")).toBe("0");
    });
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "第二次" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(2));
    const second = sendMessage.mock.calls[1]?.[0] as Record<string, unknown>;
    expect(second).not.toHaveProperty("references");
  });

  it("非空输入框按 @ 不劫持（选择器不被调用）", async () => {
    const artifacts = fakeArtifactsIpc("C:\\ws\\note.txt");
    await openSession(fakeIpc(), artifacts);
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "abc" } });
    fireEvent.keyDown(screen.getByTestId("composer-input"), { key: "@" });
    expect(artifacts.pick).not.toHaveBeenCalled();
  });

  it("chip 移除：移除后发送不带该引用", async () => {
    const sendMessage = vi.fn(async (input: Parameters<SessionIpc["sendMessage"]>[0]) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    }));
    const artifacts = fakeArtifactsIpc("C:\\ws\\note.txt");
    await openSession(fakeIpc({ sendMessage }), artifacts);
    fireEvent.keyDown(screen.getByTestId("composer-input"), { key: "@" });
    await screen.findByTestId("file-reference-chip", {}, { timeout: 5000 });
    fireEvent.click(screen.getByTestId("file-reference-remove"));
    await waitFor(() => {
      expect(screen.getByTestId("file-reference").getAttribute("data-count")).toBe("0");
    });
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "无引用" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1));
    expect(sendMessage.mock.calls[0]?.[0]).not.toHaveProperty("references");
  });

  it("取消选择器（null）不添加 chip", async () => {
    const artifacts = fakeArtifactsIpc(null);
    await openSession(fakeIpc(), artifacts);
    fireEvent.keyDown(screen.getByTestId("composer-input"), { key: "@" });
    await waitFor(() => expect(artifacts.pick).toHaveBeenCalledTimes(1));
    expect(screen.getByTestId("file-reference").getAttribute("data-count")).toBe("0");
  });
});
