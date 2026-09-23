/**
 * EventStore 集成测试（M3-01 DoD3）：10 会话 × 50 delta/s 批处理无重复渲染；
 * `gap=true` 可补齐（真实 React 订阅 + zustand）。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore, type EventBackfillSource } from "./eventStore";
import { useSessionEvents } from "./useSessionEvents";

const SESSIONS = Array.from({ length: 10 }, (_, index) => `sess-${index}`);

let idCounter = 0;

function makeEvent(sessionId: string, seq: number): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: sessionId,
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

function SessionProbe({
  store,
  sessionId,
  onRender,
}: {
  store: EventStore;
  sessionId: string;
  onRender?: (sessionId: string) => void;
}) {
  const state = useSessionEvents(store, sessionId);
  onRender?.(sessionId);
  return (
    <ul
      data-testid={`session-${sessionId}`}
      data-gap={String(state.gap)}
      data-last-seq={String(state.lastSeq)}
    >
      {state.events.map((event) => (
        <li key={event.id} data-seq={event.seq}>
          {event.seq}
        </li>
      ))}
    </ul>
  );
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("EventStore 集成（M3-01 DoD3）", () => {
  it("10 会话 × 50 delta/s：16ms 批处理，无重复渲染且逐条不丢", () => {
    vi.useFakeTimers();
    const store = new EventStore({ backfill: pageSource() });
    const renders = new Map<string, number>();

    render(
      <>
        {SESSIONS.map((sessionId) => (
          <SessionProbe
            key={sessionId}
            store={store}
            sessionId={sessionId}
            onRender={(id) =>
              renders.set(id, (renders.get(id) ?? 0) + 1)
            }
          />
        ))}
      </>,
    );

    const perSessionPerSecond = 50;
    const seconds = 5;
    for (let second = 0; second < seconds; second += 1) {
      act(() => {
        for (const sessionId of SESSIONS) {
          for (let index = 0; index < perSessionPerSecond; index += 1) {
            const seq = second * perSessionPerSecond + index + 1;
            store.ingest(makeEvent(sessionId, seq));
          }
        }
        vi.advanceTimersByTime(1_000);
      });
    }

    const totalPerSession = perSessionPerSecond * seconds;
    for (const sessionId of SESSIONS) {
      const list = screen.getByTestId(`session-${sessionId}`);
      const items = [...list.querySelectorAll("li")];
      expect(items).toHaveLength(totalPerSession);
      const seqs = items.map((item) => Number(item.getAttribute("data-seq")));
      expect(new Set(seqs).size).toBe(totalPerSession);
      expect(seqs).toEqual(
        Array.from({ length: totalPerSession }, (_, index) => index + 1),
      );
      expect(list.getAttribute("data-gap")).toBe("false");
      expect(list.getAttribute("data-last-seq")).toBe(String(totalPerSession));
      // 无重复渲染：渲染次数受 16ms 批处理约束（远小于事件数）。
      expect(renders.get(sessionId) ?? 0).toBeLessThanOrEqual(seconds + 2);
    }
    store.dispose();
  });

  it("gap=true 可补齐：缺口经补读收敛，渲染最终一致且无重复", async () => {
    const sessionId = "sess-gap";
    const missing = [makeEvent(sessionId, 6), makeEvent(sessionId, 7)];
    const source = pageSource({
      backfill: vi.fn(async (_sessionId, lastSeq) =>
        lastSeq >= 5
          ? { events: missing.filter((event) => event.seq > lastSeq), complete: true }
          : { events: [], complete: true },
      ),
    });
    const store = new EventStore({ backfill: source });
    render(<SessionProbe store={store} sessionId={sessionId} />);

    act(() => {
      for (let seq = 1; seq <= 5; seq += 1) {
        store.ingest(makeEvent(sessionId, seq));
      }
      store.flush();
      store.ingest(makeEvent(sessionId, 8));
      store.flush();
    });

    const list = screen.getByTestId(`session-${sessionId}`);
    expect(list.getAttribute("data-gap")).toBe("true");

    await vi.waitFor(() => {
      expect(
        screen
          .getByTestId(`session-${sessionId}`)
          .getAttribute("data-last-seq"),
      ).toBe("8");
    });
    const items = [...list.querySelectorAll("li")];
    const seqs = items.map((item) => Number(item.getAttribute("data-seq")));
    expect(seqs).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
    expect(new Set(seqs).size).toBe(8);
    expect(list.getAttribute("data-gap")).toBe("false");
    expect(source.backfill).toHaveBeenCalledWith(sessionId, 5);
    store.dispose();
  });
});
