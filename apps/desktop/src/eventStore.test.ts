/**
 * EventStore 单测（M3-01 DoD2）：去重 / 乱序排序 / last_seq 补读 / 缺口 >10k /
 * 16ms 批处理 / 本地入站队列有界。
 */
import type { AetherEvent } from "@aether/protocol";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  DEFAULT_RELOAD_LIMIT,
  EventStore,
  READBACK_GAP_LIMIT,
  ReadbackGapTooLargeError,
  type EventBackfillSource,
} from "./eventStore";

const SESSION_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";

let idCounter = 0;

function makeEvent(
  sessionId: string,
  seq: number,
  overrides: Partial<AetherEvent> = {},
): AetherEvent {
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
    ...overrides,
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

const stores: EventStore[] = [];

function newStore(
  options: ConstructorParameters<typeof EventStore>[0] = {},
): EventStore {
  const store = new EventStore(options);
  stores.push(store);
  return store;
}

afterEach(() => {
  for (const store of stores.splice(0)) {
    store.dispose();
  }
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("EventStore（M3-01 DoD2）", () => {
  it("去重：evt.id 与 (session, seq) 重复均只保留一条", () => {
    const store = newStore({ backfill: pageSource() });
    const first = makeEvent(SESSION_A, 1);
    const duplicateId = { ...makeEvent(SESSION_A, 2), id: first.id };
    const duplicateSeq = makeEvent(SESSION_A, 1);

    store.ingest(first);
    store.ingest(duplicateId);
    store.ingest(duplicateSeq);
    store.flush();

    const state = store.getState(SESSION_A);
    expect(state.events).toHaveLength(1);
    expect(state.events[0]?.id).toBe(first.id);
    expect(state.lastSeq).toBe(1);
  });

  it("乱序排序：缺口事件在水位推进后按 seq 有序补齐", () => {
    const store = newStore();
    store.ingest(makeEvent(SESSION_A, 3));
    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 2));
    store.flush();

    const state = store.getState(SESSION_A);
    expect(state.events.map((event) => event.seq)).toEqual([1, 2, 3]);
    expect(state.lastSeq).toBe(3);
    expect(state.gap).toBe(false);
  });

  it("last_seq 补读：缺口 ≤10k 自动补读并清零 gap", async () => {
    const source = pageSource({
      backfill: vi.fn(async (_sessionId, lastSeq) =>
        lastSeq === 1
          ? {
              events: [makeEvent(SESSION_A, 2), makeEvent(SESSION_A, 3)],
              complete: true,
            }
          : { events: [], complete: true },
      ),
    });
    const store = newStore({ backfill: source });

    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 4));
    store.flush();

    expect(store.getState(SESSION_A).gap).toBe(true);
    await vi.waitFor(() => {
      expect(store.getState(SESSION_A).lastSeq).toBe(4);
    });
    const state = store.getState(SESSION_A);
    expect(state.gap).toBe(false);
    expect(state.events.map((event) => event.seq)).toEqual([1, 2, 3, 4]);
    expect(source.backfill).toHaveBeenCalledWith(SESSION_A, 1);
  });

  it("缺口 >10k：拒绝自动补发；确认后清空缓存并加载最近 N 条（默认 500，可配置）", async () => {
    const latest = [makeEvent(SESSION_A, 901), makeEvent(SESSION_A, 902)];
    const pageLatest = vi.fn(async () => ({ events: latest, complete: true }));
    const source = pageSource({ pageLatest });
    const store = newStore({ backfill: source });

    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 1 + READBACK_GAP_LIMIT + 1));
    store.flush();

    let state = store.getState(SESSION_A);
    expect(state.historyTooLarge).toBe(true);
    expect(state.gap).toBe(true);
    expect(source.backfill).not.toHaveBeenCalled();

    await store.confirmReload(SESSION_A);
    expect(pageLatest).toHaveBeenCalledWith(SESSION_A, DEFAULT_RELOAD_LIMIT);
    state = store.getState(SESSION_A);
    expect(state.historyTooLarge).toBe(false);
    expect(state.gap).toBe(false);
    expect(state.reloadPending).toBe(false);
    expect(state.error).toBeNull();
    expect(state.events.map((event) => event.seq)).toEqual([901, 902]);
    expect(state.lastSeq).toBe(902);

    // 可配置重载条数。
    const customPage = vi.fn(async () => ({ events: [], complete: true }));
    const custom = newStore({
      backfill: pageSource({ pageLatest: customPage }),
      reloadLimit: 200,
    });
    custom.ingest(makeEvent(SESSION_A, 1));
    custom.ingest(makeEvent(SESSION_A, 1 + READBACK_GAP_LIMIT + 1));
    custom.flush();
    await custom.confirmReload(SESSION_A);
    expect(customPage).toHaveBeenCalledWith(SESSION_A, 200);
    expect(custom.getState(SESSION_A).reloadLimit).toBe(200);
  });

  it("16ms 批处理：窗口内合并为一次发布（D8 上限 60 次/秒）", () => {
    vi.useFakeTimers();
    const onFlush = vi.fn();
    const store = newStore({ backfill: pageSource(), onFlush });
    const api = store.session(SESSION_A);

    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 2));
    expect(api.getState().events).toHaveLength(0);
    expect(onFlush).not.toHaveBeenCalled();

    vi.advanceTimersByTime(15);
    expect(api.getState().events).toHaveLength(0);

    vi.advanceTimersByTime(1);
    expect(onFlush).toHaveBeenCalledTimes(1);
    expect(onFlush).toHaveBeenCalledWith(SESSION_A, 2);
    expect(api.getState().events.map((event) => event.seq)).toEqual([1, 2]);
  });

  it("本地入站队列有界：超限丢弃 + gap 标记，不阻塞 ingest", () => {
    const store = newStore({ maxBufferedEvents: 4 });
    const api = store.session(SESSION_A);

    for (let seq = 1; seq <= 10; seq += 1) {
      expect(() => store.ingest(makeEvent(SESSION_A, seq))).not.toThrow();
    }
    store.flush();

    const state = api.getState();
    expect(state.events.map((event) => event.seq)).toEqual([1, 2, 3, 4]);
    expect(state.gap).toBe(true);
  });

  it("多会话隔离：不同会话的水位/缺口互不影响", () => {
    const store = newStore();
    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_B, 5));
    store.ingest(makeEvent(SESSION_A, 2));
    store.flush();

    expect(store.getState(SESSION_A).lastSeq).toBe(2);
    expect(store.getState(SESSION_A).gap).toBe(false);
    expect(store.getState(SESSION_B).lastSeq).toBe(0);
    expect(store.getState(SESSION_B).gap).toBe(true);
    expect(store.sessionIds().sort()).toEqual([SESSION_A, SESSION_B].sort());
  });

  it("补读源抛 readback_gap_too_large（核心同码）→ 转为「历史消息过多」", async () => {
    const source = pageSource({
      backfill: vi.fn(async () => {
        throw new ReadbackGapTooLargeError(10_500);
      }),
    });
    const store = newStore({ backfill: source });
    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 3));
    store.flush();

    await vi.waitFor(() => {
      expect(store.getState(SESSION_A).historyTooLarge).toBe(true);
    });
    expect(store.getState(SESSION_A).gap).toBe(true);
    expect(store.getState(SESSION_A).error).toBeNull();
  });

  it("补读续读：单页不足（complete=false）时继续拉取直至收敛", async () => {
    const backfill = vi
      .fn()
      .mockImplementationOnce(async () => ({
        events: [makeEvent(SESSION_A, 2)],
        complete: false,
      }))
      .mockImplementationOnce(async () => ({
        events: [makeEvent(SESSION_A, 3)],
        complete: true,
      }));
    const store = newStore({ backfill: pageSource({ backfill }) });
    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 4));
    store.flush();

    await vi.waitFor(() => {
      expect(store.getState(SESSION_A).lastSeq).toBe(4);
    });
    expect(store.getState(SESSION_A).events.map((event) => event.seq)).toEqual([
      1, 2, 3, 4,
    ]);
    expect(store.getState(SESSION_A).gap).toBe(false);
    expect(backfill.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("重载失败：pageLatest 抛错时保留状态并展示错误（不崩溃）", async () => {
    const pageLatest = vi.fn(async () => {
      throw new Error("补读通道不可用");
    });
    const store = newStore({ backfill: pageSource({ pageLatest }) });
    store.ingest(makeEvent(SESSION_A, 1));
    store.ingest(makeEvent(SESSION_A, 1 + READBACK_GAP_LIMIT + 1));
    store.flush();

    await store.confirmReload(SESSION_A);
    const state = store.getState(SESSION_A);
    expect(state.historyTooLarge).toBe(true);
    expect(state.reloadPending).toBe(false);
    expect(state.error).toContain("补读通道不可用");
  });
});
