/**
 * M7-02/ADR-022：能力路由与并发闸门（前端）。
 *
 * - 自动路由开关：开启后 `session_create` 不带 `runtime_id`（缺省 = 自动路由）；
 *   手动点选运行时即退出自动路由（手动优先）；
 * - `no_candidate_runtime` 结构化错误呈现（`workbench-error` + `data-code`）；
 * - `session_send` 响应 `queue: { position, limit }` → `queue-position` 展示；
 *   `run.started` 到达（run 派发）后隐藏；无排队不渲染。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

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

let idCounter = 0;

function event(seq: number, type: string, payload: unknown, runId: string | null): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: runId,
    runtime_id: "claude-code",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

function runtime(overrides: Partial<RuntimeInfo>): RuntimeInfo {
  return {
    id: "claude-code",
    name: "Claude Code",
    kind: "acp",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: [],
    enabled: true,
    status: "cold",
    status_reason: null,
    ...overrides,
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
    listRuntimes: vi.fn(async () => []),
    listSessions: vi.fn(async () => []),
    createSession: vi.fn(async (input) =>
      sessionSummary("idle", {
        runtime_id: input.runtime_id ?? "claude-code",
        title: input.title,
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
      interrupted_run: null,
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

async function openSession(ipc: SessionIpc): Promise<EventStore> {
  const store = newStore();
  render(<SessionWorkbench store={store} ipc={ipc} />);
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
  return store;
}

describe("M7-02 自动路由（ADR-022 §2.1#1/#4）", () => {
  it("自动路由开关：开启后 session_create 不带 runtime_id；手动点选即退出自动路由", async () => {
    const createSession = vi.fn(async (input) =>
      sessionSummary("idle", { runtime_id: input.runtime_id ?? "codex" }),
    );
    const ipc = fakeIpc({
      listRuntimes: async () => [
        runtime({ id: "claude-code", name: "Claude Code" }),
        runtime({ id: "codex", name: "Codex" }),
      ],
      createSession,
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findAllByTestId("runtime-option");

    // 默认关闭；开启后 create 不带 runtime_id（后端自动路由）。
    const toggle = screen.getByTestId("runtime-auto-route");
    expect(toggle.getAttribute("data-auto")).toBe("false");
    fireEvent.click(toggle);
    expect(screen.getByTestId("runtime-auto-route").getAttribute("data-auto")).toBe("true");
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    const autoInput = createSession.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(autoInput).not.toHaveProperty("runtime_id");
    expect(autoInput.title).toBeTruthy();

    // 手动点选运行时 → 自动路由退出（手动优先），create 带 runtime_id。
    expect(screen.getByTestId("runtime-auto-route").getAttribute("data-auto")).toBe("true");
    fireEvent.click(screen.getAllByTestId("runtime-option")[1]!);
    expect(screen.getByTestId("runtime-auto-route").getAttribute("data-auto")).toBe("false");
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(createSession.mock.calls[1]?.[0]).toMatchObject({ runtime_id: "codex" });
  });

  it("no_candidate_runtime 结构化错误呈现（data-code + message）", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({})],
      createSession: async () => {
        throw {
          code: "no_candidate_runtime",
          message: "没有可用的运行时：未注册任何运行时或全部已禁用；已注册运行时：（无）；请手动选择运行时",
        };
      },
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findAllByTestId("runtime-option");
    fireEvent.click(screen.getByTestId("session-create-submit"));
    const error = await screen.findByTestId("workbench-error");
    expect(error.getAttribute("data-code")).toBe("no_candidate_runtime");
    expect(error.textContent).toContain("没有可用的运行时");
  });
});

describe("M7-02 闸门排队位置（ADR-022 附录 A-5）", () => {
  it("ack.queue → queue-position 展示（data-position/data-limit）；run.started 后隐藏", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({ status: "ready" })],
      listSessions: async () => [sessionSummary("idle")],
      sendMessage: vi.fn(async (input) => ({
        session_id: input.session_id,
        message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
        run_id: RUN_1,
        queued: false,
        duplicate: false,
        // M7-02/ADR-022：超闸门排队（8GB 模拟 limit=4，全局 FIFO 第 3 位）。
        queue: { position: 3, limit: 4 },
      })),
    });
    const store = await openSession(ipc);

    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "排队消息" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    const notice = await screen.findByTestId("queue-position");
    expect(notice.getAttribute("data-position")).toBe("3");
    expect(notice.getAttribute("data-limit")).toBe("4");
    expect(notice.textContent).toContain("第 3 位");
    expect(notice.textContent).toContain("并发上限 4");

    // run 派发（run.started）→ 排队位置隐藏（不再排队）。
    act(() => {
      store.ingest(event(1, "run.started", { run_id: RUN_1 }, RUN_1));
      store.flush();
    });
    await waitFor(() => expect(screen.queryByTestId("queue-position")).toBeNull());
  });

  it("无排队（ack 无 queue）不渲染 queue-position", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({ status: "ready" })],
      listSessions: async () => [sessionSummary("idle")],
    });
    await openSession(ipc);
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "即时消息" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(screen.getByTestId("composer-send")).toBeTruthy());
    expect(screen.queryByTestId("queue-position")).toBeNull();
  });
});
