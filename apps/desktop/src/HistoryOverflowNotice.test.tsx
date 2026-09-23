/**
 * 「历史消息过多」提示组件测试（M3-01 DoD2）。
 */
import type { AetherEvent } from "@aether/protocol";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { HistoryOverflowNotice } from "./HistoryOverflowNotice";
import {
  EventStore,
  READBACK_GAP_LIMIT,
  type EventBackfillSource,
} from "./eventStore";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

let idCounter = 0;

function makeEvent(seq: number): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: null,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type: "message.delta",
    payload: { seq },
  };
}

function pageSource(
  overrides: Partial<EventBackfillSource> = {},
): EventBackfillSource {
  return {
    backfill: vi.fn(async () => ({ events: [], complete: true })),
    pageLatest: vi.fn(async () => ({ events: [], complete: true })),
    ...overrides,
  };
}

function triggerOverflow(store: EventStore): void {
  store.ingest(makeEvent(1));
  store.ingest(makeEvent(1 + READBACK_GAP_LIMIT + 1));
  store.flush();
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("HistoryOverflowNotice（M3-01 DoD2）", () => {
  it("缺口 >10k 时显示「历史消息过多，请关闭并重新打开会话」", () => {
    const store = new EventStore({ backfill: pageSource() });
    triggerOverflow(store);

    render(<HistoryOverflowNotice store={store} sessionId={SESSION} />);
    const notice = screen.getByTestId("history-overflow");
    expect(notice.textContent).toContain("历史消息过多，请关闭并重新打开会话");
    expect(notice.textContent).toContain("重新加载最近 500 条");
    store.dispose();
  });

  it("用户确认后清空缓存并加载最近 N 条；提示消失（不重启核心/应用）", async () => {
    const latest = [makeEvent(1001), makeEvent(1002)];
    const pageLatest = vi.fn(async () => ({ events: latest, complete: true }));
    const store = new EventStore({ backfill: pageSource({ pageLatest }) });
    triggerOverflow(store);

    render(<HistoryOverflowNotice store={store} sessionId={SESSION} />);
    fireEvent.click(screen.getByTestId("history-reload"));

    await vi.waitFor(() => {
      expect(pageLatest).toHaveBeenCalledWith(SESSION, 500);
      expect(store.getState(SESSION).historyTooLarge).toBe(false);
    });
    // 组件在补齐后不再渲染提示。
    expect(screen.queryByTestId("history-overflow")).toBeNull();
    expect(store.getState(SESSION).events.map((event) => event.seq)).toEqual([
      1001, 1002,
    ]);
    store.dispose();
  });

  it("补读通道未接线（M3-01 生产缺口）时展示结构化错误，不崩溃", async () => {
    const store = new EventStore();
    triggerOverflow(store);

    render(<HistoryOverflowNotice store={store} sessionId={SESSION} />);
    fireEvent.click(screen.getByTestId("history-reload"));

    expect(await screen.findByTestId("history-reload-error")).toBeTruthy();
    expect(screen.getByTestId("history-reload-error").textContent).toContain(
      "补读通道未接线",
    );
    store.dispose();
  });
});
