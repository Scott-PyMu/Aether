/**
 * 生产补读路径 E2E（M3-02 属主承接项；Gate 3 核验项）：
 *
 * 真实生产模块（`aetherStore` 注入的 `createMessagesPageBackfillSource` + `EventStore`
 * + `SessionWorkbench`）经 `messages_page` 假传输驱动：
 * - 核心 `readback_gap_too_large` → 同码透传 → `historyTooLarge` 提示；
 * - 用户确认 → 清缓存重载最近 N 条（默认 500）→ 消息基线 + 事件水位重建；
 * - **不重启核心、不重启应用**（草稿保留、组件不重挂载）。
 */
import type { AetherEvent } from "@aether/protocol";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { createMessagesPageBackfillSource } from "./backfillSource";
import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessageRow,
  MessagesPageInput,
  MessagesPageResult,
  SessionIpc,
  SessionSummary,
} from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const CORE_MAX_SEQ = 12_000;

/** 内存核心替身：事件表 + 消息表 + `messages_page` 语义（含缺口守卫）。 */
class FakeCore {
  readonly events: AetherEvent[] = [];
  readonly messages: MessageRow[] = [];

  seed(): void {
    for (let seq = 1; seq <= CORE_MAX_SEQ; seq += 1) {
      this.events.push({
        v: 1,
        id: `01J${String(seq).padStart(23, "0")}`,
        session_id: SESSION,
        run_id: null,
        runtime_id: "mock",
        seq,
        ts: 1_700_000_000_000 + seq,
        type: "log",
        payload: { level: "info", message: `e${seq}` },
      });
    }
    for (let index = 0; index < CORE_MAX_SEQ; index += 1) {
      this.messages.push({
        id: `01J${String(index).padStart(23, "0")}`,
        session_id: SESSION,
        run_id: null,
        client_msg_id: null,
        role: index % 2 === 0 ? "user" : "assistant",
        content: `消息 ${index}`,
        seq: index + 1,
        created_at: 1_700_000_000_000 + index,
      });
    }
  }

  messagesPage(input: MessagesPageInput): MessagesPageResult {
    const maxSeq = this.events.at(-1)?.seq ?? null;
    const limit = input.limit ?? 500;
    if (input.last_seq !== undefined) {
      const gap = (maxSeq ?? 0) - input.last_seq;
      if (gap > 10_000) {
        throw { code: "readback_gap_too_large", message: "缺口过大" };
      }
      const page = this.events.filter((event) => event.seq > input.last_seq!);
      return {
        session_id: SESSION,
        last_seq: input.last_seq,
        max_seq: maxSeq,
        events: page.slice(0, limit),
        complete: page.length <= limit,
      };
    }
    return {
      session_id: SESSION,
      max_seq: maxSeq,
      events: this.events.slice(-limit),
      messages: this.messages.slice(-limit),
      complete: true,
    };
  }
}

function summary(): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "补读会话",
    status: "idle",
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
  };
}

function harness(core: FakeCore): { store: EventStore; ipc: SessionIpc } {
  const store = new EventStore({
    // 生产补读源 + 假传输（同一代码路径：backfillSource.ts）。
    backfill: createMessagesPageBackfillSource(async (input) => core.messagesPage(input)),
  });
  const ipc = {
    listRuntimes: vi.fn(async () => []),
    listSessions: vi.fn(async () => [summary()]),
    createSession: vi.fn(),
    sendMessage: vi.fn(),
    interruptSession: vi.fn(),
    disposeSession: vi.fn(),
    messagesPage: vi.fn(async (input: MessagesPageInput) => core.messagesPage(input)),
  } as unknown as SessionIpc;
  return { store, ipc };
}

const stores: EventStore[] = [];

afterEach(() => {
  cleanup();
  for (const store of stores.splice(0)) {
    store.dispose();
  }
  vi.restoreAllMocks();
});

describe("生产补读路径 E2E（M3-02 属主承接项）", () => {
  it("核心 readback_gap_too_large 同码透传 → 确认重载最近 500 条（不重启核心/应用）", async () => {
    const core = new FakeCore();
    core.seed();
    const { store, ipc } = harness(core);
    stores.push(store);

    render(<SessionWorkbench store={store} ipc={ipc} />);
    fireEvent.click(await screen.findByTestId("session-item"));
    const composer = await screen.findByTestId("composer-input");

    // 模拟消费者落后：收到 seq 1..3 后直接收到 seq 9_000（本地缺口 8_997 ≤ 10k
    // → 触发 backfill；核心侧 max=12_000 → 缺口 11_997 > 10k → 同码透传）。
    fireEvent.change(composer, { target: { value: "未发送草稿" } });
    for (const seq of [1, 2, 3, 9_000]) {
      store.ingest(core.events[seq - 1]!);
    }
    store.flush();

    const notice = await screen.findByTestId("history-overflow");
    expect(notice.textContent).toContain("历史消息过多");
    expect(store.getState(SESSION).historyTooLarge).toBe(true);

    // 用户确认：清缓存 + 重载最近 N 条（默认 500）。
    fireEvent.click(screen.getByTestId("history-reload"));
    await waitFor(() => {
      expect(screen.queryByTestId("history-overflow")).toBeNull();
    });

    const state = store.getState(SESSION);
    expect(state.historyTooLarge).toBe(false);
    expect(state.gap).toBe(false);
    expect(state.events).toHaveLength(500);
    expect(state.lastSeq).toBe(CORE_MAX_SEQ);
    expect(ipc.messagesPage).toHaveBeenCalledWith({ session_id: SESSION, limit: 500 });

    // 消息基线重建：最近一页首条可见；滚动到底可见最新一条。
    const list = await screen.findByTestId("message-list");
    await waitFor(() => {
      expect(list.getAttribute("data-total-items")).toBe("500");
    });
    expect(screen.getByText("消息 11500")).toBeTruthy();
    fireEvent.scroll(list, {
      target: { scrollTop: 500 * 96 - 480 },
    });
    expect(screen.getByText("消息 11999")).toBeTruthy();

    // 不重启核心/应用：草稿保留（组件未重挂载），补读源未触发任何重建。
    expect((composer as HTMLTextAreaElement).value).toBe("未发送草稿");
    expect(screen.getByTestId("workbench")).toBeTruthy();
  });

  it("本地缺口 >10k：提示后确认重载（最近 N 条可配置）", async () => {
    const core = new FakeCore();
    core.seed();
    const { store, ipc } = harness(core);
    stores.push(store);

    render(<SessionWorkbench store={store} ipc={ipc} />);
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");

    for (const seq of [1, 2, 3, 12_001]) {
      store.ingest(core.events[seq - 1] ?? { ...core.events[0]!, seq, id: `01J${String(seq).padStart(23, "0")}` });
    }
    store.flush();
    await screen.findByTestId("history-overflow");

    fireEvent.click(screen.getByTestId("history-reload"));
    await waitFor(() => {
      expect(store.getState(SESSION).historyTooLarge).toBe(false);
    });
    expect(store.getState(SESSION).events).toHaveLength(500);
  });
});
