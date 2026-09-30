/**
 * 会话工作台集成测试（M3-02 DoD1/DoD2/DoD3）：
 * - DoD1：创建 → 发送 → 流式渲染 → 中断 → 状态正确（事件流驱动，React 真实订阅）；
 * - DoD2：运行时选择器展示 cold/ready/degraded/disabled+reason 与能力徽标；
 * - DoD3：会话级模型覆盖透传（`session.create` 参数断言）。
 */
import type { AetherEvent } from "@aether/protocol";
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

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_1 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";
const RUN_2 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1S";

let idCounter = 0;

function event(
  seq: number,
  type: string,
  payload: unknown,
  runId: string | null,
): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: runId,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

function runtime(overrides: Partial<RuntimeInfo>): RuntimeInfo {
  return {
    id: "mock",
    name: "Mock",
    kind: "mock",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: [],
    enabled: true,
    status: "cold",
    status_reason: null,
    ...overrides,
  };
}

function sessionSummary(
  status: SessionStatus,
  overrides: Partial<SessionSummary> = {},
): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
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
    createSession: vi.fn(async (input) => sessionSummary("idle", {
      runtime_id: input.runtime_id,
      title: input.title,
      model: input.model ?? null,
    })),
    sendMessage: vi.fn(async (input) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      interrupted_run: RUN_2,
    })),
    disposeSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async () => emptyPage()),
    // M3-06：一键重放命令面（本文件不消费；缺省替身保持接口完整）。
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

describe("SessionWorkbench（M3-02）", () => {
  it("DoD2：运行时选择器展示状态、原因与能力徽标", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => [
        runtime({ id: "codex", name: "Codex", status: "ready", capabilities: ["session.send", "tools.list"] }),
        runtime({ id: "claude-code", name: "Claude Code", status: "degraded", status_reason: "crash_loop" }),
        runtime({ id: "deepseek-harness", name: "DSH", status: "cold" }),
        runtime({ id: "mock", name: "Mock", status: "disabled", status_reason: "untrusted" }),
      ],
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);

    const options = await screen.findAllByTestId("runtime-option");
    expect(options).toHaveLength(4);
    expect(options.map((option) => option.getAttribute("data-status"))).toEqual([
      "ready",
      "degraded",
      "cold",
      "disabled",
    ]);
    const reasons = screen.getAllByTestId("runtime-reason").map((node) => node.textContent);
    expect(reasons).toEqual(["crash_loop", "untrusted"]);
    const badges = screen.getAllByTestId("capability-badge").map((node) => node.textContent);
    expect(badges).toEqual(["session.send", "tools.list"]);
    // 默认选中第一个 ready 运行时。
    expect(options[0]?.getAttribute("data-selected")).toBe("true");
  });

  it("DoD3：会话级模型覆盖透传（session.create 参数断言）", async () => {
    const created: SessionSummary[] = [];
    const createSession = vi.fn(async (input) => {
      const session = sessionSummary("idle", {
        runtime_id: input.runtime_id,
        title: input.title,
        model: input.model ?? null,
      });
      created.push(session);
      return session;
    });
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({ status: "ready" })],
      // 返回新数组（不返回被原地 push 的同一引用）：`sessions` 状态按不可变语义更新，
      // M3-12 分组 useMemo 才会按依赖变化重算。
      listSessions: async () => [...created],
      createSession,
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    await screen.findAllByTestId("runtime-option");

    fireEvent.change(screen.getByTestId("session-title-input"), {
      target: { value: "覆盖模型会话" },
    });
    fireEvent.change(screen.getByTestId("session-model-input"), {
      target: { value: "deepseek-v4-pro" },
    });
    fireEvent.click(screen.getByTestId("session-create-submit"));

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession).toHaveBeenCalledWith({
      runtime_id: "mock",
      title: "覆盖模型会话",
      model: "deepseek-v4-pro",
      // M3-10/ADR-010：会话级思考深度随 `session_create` 透传（本次未调整 → 缺省 2）。
      thinking_depth: 2,
    });
    const item = await screen.findByTestId("session-item");
    expect(item.getAttribute("data-active")).toBe("true");
    expect(screen.getByTestId("session-item-model").textContent).toBe(
      "deepseek-v4-pro",
    );
    // T1 埋点：创建耗时锚点已记录（数值由 M4-04 基准读取）。
    const latency = await screen.findByTestId("create-latency-ms");
    expect(latency.getAttribute("data-ms")).toMatch(/^\d+$/);
  });

  it("DoD1：创建 → 发送 → 流式渲染 → 中断 → 状态正确", async () => {
    let sessionStatus: SessionStatus = "idle";
    const sendMessage = vi
      .fn()
      .mockResolvedValueOnce({
        session_id: SESSION,
        message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
        run_id: RUN_1,
        queued: false,
        duplicate: false,
      })
      .mockResolvedValueOnce({
        session_id: SESSION,
        message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N",
        run_id: RUN_2,
        queued: false,
        duplicate: false,
      });
    const interruptSession = vi.fn(async () => {
      sessionStatus = "idle";
      return { session_id: SESSION, interrupted_run: RUN_2 };
    });
    const ipc = fakeIpc({
      listRuntimes: async () => [runtime({ status: "ready" })],
      listSessions: async () => [sessionSummary(sessionStatus)],
      sendMessage,
      interruptSession,
    });
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} />);

    // 创建/选择：会话列表出现后激活。
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");

    // 发送：ack 快路径 → 乐观用户气泡 + 幂等键（ULID 26 位）。
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "第一条消息" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1));
    const payload = sendMessage.mock.calls[0]?.[0] as {
      text: string;
      client_msg_id: string;
    };
    expect(payload.text).toBe("第一条消息");
    expect(payload.client_msg_id).toMatch(/^[0-9A-HJKMNP-TV-Z]{26}$/);

    // 流式渲染：run.started → delta → 终稿。
    act(() => {
      store.ingest(event(1, "run.started", { run_id: RUN_1 }, RUN_1));
      store.flush();
    });
    act(() => {
      store.ingest(event(2, "message.delta", { message_id: "m-1", text: "流式" }, RUN_1));
      store.ingest(event(3, "message.delta", { message_id: "m-1", text: "回答" }, RUN_1));
      store.flush();
    });
    const streamingBubble = await screen.findByText("流式回答");
    expect(streamingBubble).toBeTruthy();
    const assistantBubble = screen
      .getAllByTestId("message-bubble")
      .find((bubble) => bubble.getAttribute("data-role") === "assistant");
    expect(assistantBubble?.getAttribute("data-streaming")).toBe("true");
    expect(screen.getByTestId("streaming-indicator")).toBeTruthy();
    expect(screen.getByTestId("run-status").textContent).toContain("run 运行中");

    act(() => {
      store.ingest(
        event(
          4,
          "message.completed",
          {
            message: {
              id: "m-1",
              session_id: SESSION,
              run_id: RUN_1,
              role: "assistant",
              content: "流式回答",
              created_at: 1,
            },
            usage: null,
          },
          RUN_1,
        ),
      );
      store.ingest(event(5, "run.completed", { run_id: RUN_1, usage: null }, RUN_1));
      store.flush();
    });
    await waitFor(() => {
      const bubble = screen
        .getAllByTestId("message-bubble")
        .find((node) => node.getAttribute("data-role") === "assistant");
      expect(bubble?.getAttribute("data-streaming")).toBe("false");
    });
    expect(screen.queryByTestId("streaming-indicator")).toBeNull();

    // 第二次发送 → 中断：run.cancelled + 状态条回 idle/cancelled。
    fireEvent.change(screen.getByTestId("composer-input"), {
      target: { value: "第二条消息" },
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(2));
    act(() => {
      store.ingest(event(6, "run.started", { run_id: RUN_2 }, RUN_2));
      store.ingest(event(7, "message.delta", { message_id: "m-2", text: "半截" }, RUN_2));
      store.flush();
    });
    expect(screen.getByTestId("run-status").textContent).toContain("run 运行中");

    fireEvent.click(screen.getByTestId("interrupt"));
    await waitFor(() => expect(interruptSession).toHaveBeenCalledWith(SESSION));
    act(() => {
      store.ingest(
        event(8, "run.cancelled", { run_id: RUN_2, reason: "user_interrupt" }, RUN_2),
      );
      store.flush();
    });
    await waitFor(() => {
      expect(screen.getByTestId("session-status").textContent).toBe("空闲");
      expect(screen.getByTestId("run-status").textContent).toBe("cancelled");
    });
    expect((screen.getByTestId("interrupt") as HTMLButtonElement).disabled).toBe(true);
  });

  it("无运行时/无会话时给出空态提示且发送禁用", async () => {
    const ipc = fakeIpc();
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    expect(await screen.findByTestId("runtime-list-empty")).toBeTruthy();
    expect(screen.getByTestId("session-list-empty")).toBeTruthy();
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
  });

  it("IPC 失败展示结构化错误（不崩溃）", async () => {
    const ipc = fakeIpc({
      listRuntimes: async () => {
        throw { code: "core_not_ready", message: "监督器未接线" };
      },
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    const error = await screen.findByTestId("workbench-error");
    expect(error.textContent).toContain("监督器未接线");
  });
});
