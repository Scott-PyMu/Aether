/**
 * M7-04 标签页与分屏（UI-01 完整；设计 §2.3「多视图」P1、§4#20、D8）。
 *
 * 覆盖：
 * - DoD1：标签/分屏交互（创建/切换/关闭/分屏）+ 各视图 EventStore 隔离
 *   （事件不串流、无重复渲染）；
 * - DoD3：布局持久化（settings `ui.layout`；重启恢复；损坏回退默认布局）；
 * - DoD4：后台会话状态角标与 M3-12 分组口径一致（`sessionGroupOf`）+ 事件驱动刷新。
 *
 * 锚点（UI-UX §7.3，M7-04 任务登记）：`workbench-tabs` / `session-tab` /
 * `session-tab-badge` / `session-tab-close` / `split-toggle` / `split-pane`。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import type { AetherEventListener } from "./eventBridge";
import { LAYOUT_VERSION } from "./layout";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";
import { sessionGroupOf } from "./sessionGroups";
import { UI_LAYOUT_KEY, type SettingsIpc } from "./settings";

const SESSION_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";
const SESSION_C = "01J8ZQ5R0N7W9Y8X6V4T2S0K1C";
const SESSION_D = "01J8ZQ5R0N7W9Y8X6V4T2S0K1D";
const SESSION_NEW = "01J8ZQ5R0N7W9Y8X6V4T2S0K1E";
const WAIT = { timeout: 5_000 };

let eventCounter = 0;

function event(
  sessionId: string,
  seq: number,
  type: string,
  payload: unknown,
  runId: string | null = null,
): AetherEvent {
  eventCounter += 1;
  return {
    v: 1,
    id: `01J${String(eventCounter).padStart(23, "0")}`,
    session_id: sessionId,
    run_id: runId,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

function runtime(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
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
    ...overrides,
  };
}

function session(
  id: string,
  status: SessionStatus,
  title: string,
  updatedAt = 1,
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
    listSessions: vi.fn(async () => []),
    createSession: vi.fn(async (input) =>
      session(SESSION_NEW, "idle", input.title, 99),
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
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async (input) => emptyPage(input.session_id)),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
    ...overrides,
  };
}

/** 设置 IPC 替身（`ui.layout` 持久化；返回值可注入）。 */
function fakeSettings(value: unknown): SettingsIpc & {
  get: ReturnType<typeof vi.fn>;
  set: ReturnType<typeof vi.fn>;
} {
  return {
    get: vi.fn(async () => ({ key: UI_LAYOUT_KEY, value })),
    set: vi.fn(async (key: string, next: unknown) => ({ key, value: next })),
  } as unknown as SettingsIpc & {
    get: ReturnType<typeof vi.fn>;
    set: ReturnType<typeof vi.fn>;
  };
}

function persistedLayout(value: {
  tabs: string[];
  active: string | null;
  split: string | null;
}): string {
  return JSON.stringify({ version: LAYOUT_VERSION, ...value });
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

function tabIds(): string[] {
  return screen
    .queryAllByTestId("session-tab")
    .map((tab) => tab.getAttribute("data-session-id") ?? "");
}

async function openSessionItem(sessionId: string): Promise<void> {
  const item = (await screen.findAllByTestId("session-item", {}, WAIT)).find(
    (node) => node.getAttribute("data-session-id") === sessionId,
  );
  if (!item) {
    throw new Error(`会话项不存在：${sessionId}`);
  }
  fireEvent.click(item);
}

describe("M7-04 DoD1：标签/分屏交互与 EventStore 隔离", () => {
  it("创建/切换/关闭/分屏：标签态与分屏窗格随操作正确变化", async () => {
    const sessions = [
      session(SESSION_A, "idle", "会话 A", 2),
      session(SESSION_B, "idle", "会话 B", 1),
    ];
    const ipc = fakeIpc({
      listSessions: vi.fn(async () => sessions),
      createSession: vi.fn(async (input) => {
        const created = session(SESSION_NEW, "idle", input.title, 99);
        sessions.unshift(created);
        return created;
      }),
    });
    const settings = fakeSettings("{}");
    render(
      <SessionWorkbench store={newStore()} ipc={ipc} settingsIpc={settings} />,
    );

    // 首次运行（无持久化布局）延续 P0：自动打开首个会话。
    await waitFor(() => expect(tabIds()).toEqual([SESSION_A]), WAIT);
    expect(
      screen.getByTestId("session-tab").getAttribute("data-active"),
    ).toBe("true");

    // 切换：点击会话 B → 追加标签并聚焦。
    await openSessionItem(SESSION_B);
    await waitFor(() => expect(tabIds()).toEqual([SESSION_A, SESSION_B]), WAIT);
    const tabB = screen
      .getAllByTestId("session-tab")
      .find((tab) => tab.getAttribute("data-session-id") === SESSION_B);
    expect(tabB?.getAttribute("data-active")).toBe("true");

    // 点标签 A → 切回（标签顺序不变）。
    const tabA = screen
      .getAllByTestId("session-tab")
      .find((tab) => tab.getAttribute("data-session-id") === SESSION_A);
    fireEvent.click(tabA as HTMLElement);
    await waitFor(
      () =>
        expect(
          screen
            .getAllByTestId("session-tab")
            .find((tab) => tab.getAttribute("data-session-id") === SESSION_A)
            ?.getAttribute("data-active"),
        ).toBe("true"),
      WAIT,
    );

    // 分屏：第二个标签（B）进入右窗格。
    const splitToggle = screen.getByTestId("split-toggle");
    expect(splitToggle.getAttribute("data-enabled")).toBe("true");
    fireEvent.click(splitToggle);
    await waitFor(() => {
      const pane = screen.getByTestId("split-pane");
      expect(pane.getAttribute("data-session-id")).toBe(SESSION_B);
    }, WAIT);
    expect(screen.getByTestId("split-toggle").getAttribute("data-split")).toBe("true");
    expect(screen.getByTestId("pane-primary").getAttribute("data-session-id")).toBe(
      SESSION_A,
    );

    // 创建会话 → 打开为新标签并聚焦。
    fireEvent.change(screen.getByTestId("session-title-input"), {
      target: { value: "新标签会话" },
    });
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(
      () => expect(tabIds()).toContain(SESSION_NEW),
      WAIT,
    );
    expect(
      screen
        .getAllByTestId("session-tab")
        .find((tab) => tab.getAttribute("data-session-id") === SESSION_NEW)
        ?.getAttribute("data-active"),
    ).toBe("true");

    // 关闭聚焦标签（新建）→ 聚焦相邻标签 B；B 同时是分屏会话 → 分屏关闭。
    const closeNew = screen
      .getAllByTestId("session-tab-close")
      .find((button) => button.getAttribute("data-session-id") === SESSION_NEW);
    fireEvent.click(closeNew as HTMLElement);
    await waitFor(() => expect(tabIds()).toEqual([SESSION_A, SESSION_B]), WAIT);
    expect(screen.queryByTestId("split-pane")).toBeNull();

    // 再次分屏（A 进入第二窗格），然后关闭分屏窗格标签 → 分屏随之关闭。
    fireEvent.click(screen.getByTestId("split-toggle"));
    await waitFor(() => {
      expect(screen.getByTestId("split-pane").getAttribute("data-session-id")).toBe(
        SESSION_A,
      );
    }, WAIT);
    const closeA = screen
      .getAllByTestId("session-tab-close")
      .find((button) => button.getAttribute("data-session-id") === SESSION_A);
    fireEvent.click(closeA as HTMLElement);
    await waitFor(() => expect(tabIds()).toEqual([SESSION_B]), WAIT);
    expect(screen.queryByTestId("split-pane")).toBeNull();
    expect(screen.getByTestId("split-toggle").getAttribute("data-split")).toBe(
      "false",
    );
    expect(screen.getByTestId("pane-primary").getAttribute("data-session-id")).toBe(
      SESSION_B,
    );

    // 关闭最后一个标签 → 空态（不删除会话；左栏列表仍在）。
    fireEvent.click(screen.getByTestId("session-tab-close"));
    await waitFor(() => expect(tabIds()).toEqual([]), WAIT);
    expect(screen.getByTestId("workbench-tabs-empty")).toBeTruthy();
    expect(screen.getByTestId("no-session-hint")).toBeTruthy();
    // 关闭标签不删除会话：左栏列表仍含 A/B/新建会话。
    expect(screen.getAllByTestId("session-item")).toHaveLength(3);
  });

  it("各视图 EventStore 隔离：事件不串流、后台标签不渲染、无重复气泡", async () => {
    const sessions = [
      session(SESSION_A, "running", "会话 A"),
      session(SESSION_B, "running", "会话 B"),
      session(SESSION_C, "running", "后台会话 C"),
    ];
    const ipc = fakeIpc({ listSessions: vi.fn(async () => sessions) });
    const settings = fakeSettings(
      persistedLayout({
        tabs: [SESSION_A, SESSION_B, SESSION_C],
        active: SESSION_A,
        split: SESSION_B,
      }),
    );
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} settingsIpc={settings} />);

    await waitFor(() => {
      expect(screen.getByTestId("split-pane").getAttribute("data-session-id")).toBe(
        SESSION_B,
      );
    }, WAIT);
    expect(tabIds()).toEqual([SESSION_A, SESSION_B, SESSION_C]);

    act(() => {
      store.ingest(
        event(SESSION_A, 1, "run.started", { run_id: "run-a" }, "run-a"),
      );
      store.ingest(
        event(SESSION_A, 2, "message.delta", { message_id: "m-a", text: "甲-" }, "run-a"),
      );
      store.ingest(
        event(SESSION_A, 3, "message.delta", { message_id: "m-a", text: "甲二" }, "run-a"),
      );
      store.ingest(
        event(SESSION_B, 1, "run.started", { run_id: "run-b" }, "run-b"),
      );
      store.ingest(
        event(SESSION_B, 2, "message.delta", { message_id: "m-b", text: "乙一" }, "run-b"),
      );
      store.ingest(
        event(SESSION_C, 1, "run.started", { run_id: "run-c" }, "run-c"),
      );
      store.ingest(
        event(SESSION_C, 2, "message.delta", { message_id: "m-c", text: "丙一" }, "run-c"),
      );
      store.flush();
    });

    // 数据层隔离：各会话 store 只含自己的事件。
    expect(store.getState(SESSION_A).events).toHaveLength(3);
    expect(store.getState(SESSION_B).events).toHaveLength(2);
    expect(store.getState(SESSION_C).events).toHaveLength(2);
    expect(
      store.getState(SESSION_A).events.every((item) => item.session_id === SESSION_A),
    ).toBe(true);

    // 主窗格只渲染 A 的流；分屏窗格只渲染 B 的流；后台标签 C 不渲染消息。
    const primaryBubbles = screen.getAllByTestId("message-bubble");
    expect(primaryBubbles).toHaveLength(1);
    expect(primaryBubbles[0]?.getAttribute("data-run-id")).toBe("run-a");
    expect(primaryBubbles[0]?.textContent).toContain("甲-甲二");
    expect(primaryBubbles[0]?.textContent).not.toContain("乙一");

    const splitBubbles = screen.getAllByTestId("split-message-bubble");
    expect(splitBubbles).toHaveLength(1);
    expect(splitBubbles[0]?.getAttribute("data-session-id")).toBe(SESSION_B);
    expect(splitBubbles[0]?.getAttribute("data-run-id")).toBe("run-b");
    expect(splitBubbles[0]?.textContent).toContain("乙一");
    expect(splitBubbles[0]?.textContent).not.toContain("甲-甲二");

    // 后台标签（C）的事件不产生任何气泡（无串流/重复渲染）。
    expect(screen.queryByText(/丙一/)).toBeNull();
    expect(screen.queryAllByTestId("message-bubble")).toHaveLength(1);
    expect(screen.queryAllByTestId("split-message-bubble")).toHaveLength(1);
  });
});

describe("M7-04 DoD3：布局持久化（settings ui.layout）", () => {
  it("重启恢复：读取持久化布局（标签顺序/聚焦/分屏）；显式操作写回", async () => {
    const sessions = [
      session(SESSION_A, "idle", "会话 A"),
      session(SESSION_B, "idle", "会话 B"),
    ];
    const ipc = fakeIpc({ listSessions: vi.fn(async () => sessions) });
    const settings = fakeSettings(
      persistedLayout({
        tabs: [SESSION_B, SESSION_A],
        active: SESSION_B,
        split: SESSION_A,
      }),
    );
    render(<SessionWorkbench store={newStore()} ipc={ipc} settingsIpc={settings} />);

    await waitFor(() => expect(tabIds()).toEqual([SESSION_B, SESSION_A]), WAIT);
    expect(settings.get).toHaveBeenCalledWith(UI_LAYOUT_KEY);
    expect(screen.getByTestId("pane-primary").getAttribute("data-session-id")).toBe(
      SESSION_B,
    );
    expect(screen.getByTestId("split-pane").getAttribute("data-session-id")).toBe(
      SESSION_A,
    );
    // 恢复不产生写回（仅显式操作持久化）。
    expect(settings.set).not.toHaveBeenCalled();

    // 显式切换 → 写回 `ui.layout`（active=会话 A）。
    fireEvent.click(
      screen
        .getAllByTestId("session-tab")
        .find((tab) => tab.getAttribute("data-session-id") === SESSION_A) as HTMLElement,
    );
    await waitFor(() => expect(settings.set).toHaveBeenCalled(), WAIT);
    const [key, raw] = settings.set.mock.calls.at(-1) as [string, string];
    expect(key).toBe(UI_LAYOUT_KEY);
    const saved = JSON.parse(raw) as {
      version: number;
      tabs: string[];
      active: string;
      split: string | null;
    };
    expect(saved.version).toBe(LAYOUT_VERSION);
    expect(saved.tabs).toEqual([SESSION_B, SESSION_A]);
    expect(saved.active).toBe(SESSION_A);
    // 聚焦分屏窗格会话 → 两窗格对调（split 仍为另一会话）。
    expect(saved.split).toBe(SESSION_B);
  });

  it.each([
    ["非法 JSON", "{oops"],
    ["非对象", "[]"],
    ["标签类型非法", '{"version":1,"tabs":"nope","active":null,"split":null}'],
    ["版本不符", '{"version":99,"tabs":[],"active":null,"split":null}'],
  ])("数据损坏（%s）→ 回退默认布局（空标签/无分屏，不崩溃）", async (_label, raw) => {
    const sessions = [session(SESSION_A, "idle", "会话 A")];
    const ipc = fakeIpc({ listSessions: vi.fn(async () => sessions) });
    const settings = fakeSettings(raw);
    render(<SessionWorkbench store={newStore()} ipc={ipc} settingsIpc={settings} />);

    await waitFor(() => expect(screen.getByTestId("workbench-tabs-empty")).toBeTruthy(), WAIT);
    expect(tabIds()).toEqual([]);
    expect(screen.queryByTestId("split-pane")).toBeNull();
    expect(screen.getByTestId("workbench")).toBeTruthy();
    // 左栏会话列表不受布局损坏影响。
    expect(screen.getAllByTestId("session-item")).toHaveLength(1);
    // 损坏也属于「已持久化」：不自动打开首个会话（回退默认布局语义）。
    expect(settings.set).not.toHaveBeenCalled();
  });
});

describe("M7-04 DoD4：后台会话状态角标（M3-12 分组口径）", () => {
  it("角标 data-status/data-group 与 sessionGroupOf 一致；后台标记正确；事件驱动刷新", async () => {
    let sessions = [
      session(SESSION_A, "running", "运行会话"),
      session(SESSION_B, "waiting_permission", "等待会话"),
      session(SESSION_C, "failed", "失败会话"),
      session(SESSION_D, "idle", "空闲会话"),
    ];
    const ipc = fakeIpc({ listSessions: vi.fn(async () => sessions) });
    const settings = fakeSettings(
      persistedLayout({
        tabs: [SESSION_A, SESSION_B, SESSION_C, SESSION_D],
        active: SESSION_A,
        split: SESSION_B,
      }),
    );

    let handler: ((event: AetherEvent) => void) | null = null;
    const eventListener: AetherEventListener = vi.fn(async (listener) => {
      handler = listener;
      return () => {};
    });
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={ipc}
        settingsIpc={settings}
        eventListener={eventListener}
      />,
    );

    await waitFor(() => expect(tabIds()).toHaveLength(4), WAIT);
    const tabs = () => screen.getAllByTestId("session-tab");
    const tabOf = (id: string) =>
      tabs().find((tab) => tab.getAttribute("data-session-id") === id);
    const badgeOf = (id: string) => {
      const tab = tabOf(id);
      return tab?.querySelector('[data-testid="session-tab-badge"]') ?? null;
    };

    // 与 M3-12 分组口径一致（同一 sessionGroupOf 映射）。
    for (const summary of sessions) {
      const tab = tabOf(summary.id);
      expect(tab?.getAttribute("data-status")).toBe(sessionGroupOf(summary.status));
    }
    expect(badgeOf(SESSION_A)?.getAttribute("data-group")).toBe("running");
    expect(badgeOf(SESSION_B)?.getAttribute("data-group")).toBe("waiting_permission");
    expect(badgeOf(SESSION_C)?.getAttribute("data-group")).toBe("failed");
    // 其他组（idle）不渲染角标（空组隐藏口径同 M3-12）。
    expect(badgeOf(SESSION_D)).toBeNull();

    // 后台标记：主窗格/分屏窗格之外的标签 = true。
    expect(tabOf(SESSION_A)?.getAttribute("data-background")).toBe("false");
    expect(tabOf(SESSION_B)?.getAttribute("data-background")).toBe("false");
    expect(tabOf(SESSION_C)?.getAttribute("data-background")).toBe("true");
    expect(tabOf(SESSION_D)?.getAttribute("data-background")).toBe("true");

    // `session.status_changed` 到达 → 会话列表刷新 → 角标随即更新。
    sessions = [
      session(SESSION_A, "running", "运行会话"),
      session(SESSION_B, "failed", "等待会话", 3),
      session(SESSION_C, "failed", "失败会话"),
      session(SESSION_D, "idle", "空闲会话"),
    ];
    await waitFor(() => expect(handler).not.toBeNull(), WAIT);
    act(() => {
      handler?.(
        event(SESSION_B, 1, "session.status_changed", {
          session_id: SESSION_B,
          from: "waiting_permission",
          to: "failed",
        }),
      );
    });
    await waitFor(
      () => expect(badgeOf(SESSION_B)?.getAttribute("data-group")).toBe("failed"),
      WAIT,
    );
  });
});
