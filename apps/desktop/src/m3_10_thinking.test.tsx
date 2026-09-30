/**
 * M3-10 前端集成测试：思考深度（输入区 5 档滑块；ADR-010 决策 2；UI-UX §2.4/§7.3）。
 *
 * 覆盖：
 * - DoD2 透传：`session_create` / `session_send` 携带 `thinking_depth`（缺省 2；
 *   用户调整后随两者透传）；
 * - DoD3 能力门（UI 面）：运行时未声明 `thinking_depth` → 滑块 `data-enabled=false`
 *   + 置灰 + tooltip `thinking-disabled-hint`「当前运行时不支持思考深度」；
 *   声明 → 可调；同步判定路径警告（`thinking_depth_unsupported`）非阻断展示；
 * - 回显：会话切换以 `SessionSummary.thinking_depth` 生效值回显（延迟判定路径）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_1 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";

const WAIT = { timeout: 5000 };

function runtime(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
  return {
    id: "mock",
    name: "Mock",
    kind: "mock",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: ["session.create", "session.send", "thinking_depth"],
    enabled: true,
    status: "ready",
    status_reason: null,
    ...overrides,
  };
}

function sessionSummary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "思考深度会话",
    status: "idle" as SessionStatus,
    model: null,
    thinking_depth: 2,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    ...overrides,
  };
}

function emptyPage(): MessagesPageResult {
  return {
    session_id: SESSION,
    max_seq: null,
    events: [],
    messages: [],
    complete: true,
  };
}

function fakeIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    listSessions: vi.fn(async () => []),
    createSession: vi.fn(async (input) =>
      sessionSummary({
        runtime_id: input.runtime_id,
        title: input.title,
        thinking_depth: input.thinking_depth ?? 2,
      }),
    ),
    sendMessage: vi.fn(async (input) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      interrupted_run: RUN_1,
    })),
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

async function openThinking(store: EventStore, ipc: SessionIpc) {
  render(<SessionWorkbench store={store} ipc={ipc} />);
  await screen.findByTestId("runtime-option", {}, WAIT);
  fireEvent.click(screen.getByTestId("thinking-toggle"));
  return screen.findByTestId("thinking-slider", {}, WAIT);
}

describe("思考深度（M3-10 / ADR-010）", () => {
  it("DoD3：未声明能力 → 滑块置灰 + tooltip；声明 → 可调", async () => {
    const unsupportedIpc = fakeIpc({
      listRuntimes: async () => [
        runtime({ capabilities: ["session.create", "session.send"] }),
      ],
    });
    const slider = await openThinking(newStore(), unsupportedIpc);
    expect(slider.getAttribute("data-enabled")).toBe("false");
    expect(slider.getAttribute("data-value")).toBe("2");
    expect(slider.hasAttribute("disabled")).toBe(true);
    const hint = screen.getByTestId("thinking-disabled-hint");
    expect(hint.textContent).toBe("当前运行时不支持思考深度");
    expect(screen.getByTestId("thinking-toggle").getAttribute("data-enabled")).toBe("false");

    cleanup();
    const supportedIpc = fakeIpc();
    const slider2 = await openThinking(newStore(), supportedIpc);
    expect(slider2.getAttribute("data-enabled")).toBe("true");
    expect(slider2.hasAttribute("disabled")).toBe(false);
    expect(screen.queryByTestId("thinking-disabled-hint")).toBeNull();
  });

  it("DoD2：滑块值随 session_create / session_send 透传（缺省 2）", async () => {
    let sessions: SessionSummary[] = [];
    const ipc = fakeIpc({
      listSessions: async () => sessions,
      createSession: vi.fn(async (input) => {
        const created = sessionSummary({
          id: `${SESSION}${sessions.length}`,
          runtime_id: input.runtime_id,
          title: input.title,
          thinking_depth: input.thinking_depth ?? 2,
        });
        sessions = [created, ...sessions];
        return created;
      }),
    });
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} />);
    await screen.findByTestId("runtime-option", {}, WAIT);

    // 缺省：不调整滑块 → 新建会话携带 2。
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => expect(ipc.createSession).toHaveBeenCalledTimes(1), WAIT);
    expect(ipc.createSession).toHaveBeenLastCalledWith(
      expect.objectContaining({ thinking_depth: 2 }),
    );

    // 调整到 4 → 新建会话携带 4。
    fireEvent.click(screen.getByTestId("thinking-toggle"));
    const slider = await screen.findByTestId("thinking-slider", {}, WAIT);
    fireEvent.change(slider, { target: { value: "4" } });
    expect(slider.getAttribute("data-value")).toBe("4");
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => expect(ipc.createSession).toHaveBeenCalledTimes(2), WAIT);
    expect(ipc.createSession).toHaveBeenLastCalledWith(
      expect.objectContaining({ thinking_depth: 4 }),
    );

    // 发送：本次 run 覆盖同样透传滑块当前值。
    const input = screen.getByTestId("composer-input");
    fireEvent.change(input, { target: { value: "你好" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(ipc.sendMessage).toHaveBeenCalledTimes(1), WAIT);
    expect(ipc.sendMessage).toHaveBeenCalledWith(
      expect.objectContaining({ thinking_depth: 4 }),
    );
  });

  it("DoD3：同步判定路径警告非阻断展示（thinking_depth_unsupported）", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => [
        runtime({ capabilities: ["session.create", "session.send"] }),
      ],
      createSession: vi.fn(async (input) =>
        sessionSummary({
          runtime_id: input.runtime_id,
          title: input.title,
          thinking_depth: 2,
          warnings: [
            {
              code: "thinking_depth_unsupported",
              field: "thinking_depth",
              runtime_id: "mock",
              message: "当前运行时不支持思考深度，已按默认档位运行",
            },
          ],
        }),
      ),
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findByTestId("runtime-option", {}, WAIT);
    fireEvent.click(screen.getByTestId("session-create-submit"));
    const warning = await screen.findByTestId("thinking-warning", {}, WAIT);
    expect(warning.textContent).toBe("当前运行时不支持思考深度，已按默认档位运行");
    expect(warning.getAttribute("role")).toBe("status");
  });

  it("回显：会话切换以 SessionSummary.thinking_depth 生效值回显（延迟判定路径）", async () => {
    const ipc = fakeIpc({
      listSessions: async () => [sessionSummary({ thinking_depth: 3 })],
    });
    const slider = await openThinking(newStore(), ipc);
    await waitFor(() => expect(slider.getAttribute("data-value")).toBe("3"), WAIT);
    expect(screen.getByTestId("thinking-current").textContent).toBe("极高");
  });
});
