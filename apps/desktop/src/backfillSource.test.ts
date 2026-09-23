/**
 * 生产补读源单测（M3-02 属主承接项）：`messages_page` 映射与
 * `readback_gap_too_large` 同码透传。
 */
import type { AetherEvent } from "@aether/protocol";
import { describe, expect, it, vi } from "vitest";

import {
  BACKFILL_PAGE_LIMIT,
  createMessagesPageBackfillSource,
  isReadbackGapTooLarge,
  productionBackfillSource,
} from "./backfillSource";
import { ReadbackGapTooLargeError } from "./eventStore";
import type { MessagesPageResult, SessionIpc } from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

function event(seq: number): AetherEvent {
  return {
    v: 1,
    id: `01J${String(seq).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: null,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type: "log",
    payload: { level: "info", message: `e${seq}` },
  };
}

describe("createMessagesPageBackfillSource（M3-02）", () => {
  it("backfill 以 last_seq + 分页上限调用 messages_page", async () => {
    const page = vi.fn(
      async (): Promise<MessagesPageResult> => ({
        session_id: SESSION,
        last_seq: 5,
        max_seq: 8,
        events: [event(6), event(7), event(8)],
        complete: true,
      }),
    );
    const source = createMessagesPageBackfillSource(page);
    const result = await source.backfill(SESSION, 5);
    expect(page).toHaveBeenCalledWith({
      session_id: SESSION,
      last_seq: 5,
      limit: BACKFILL_PAGE_LIMIT,
    });
    expect(result.events.map((item) => item.seq)).toEqual([6, 7, 8]);
    expect(result.complete).toBe(true);
  });

  it("pageLatest 不携带 last_seq（最近一页）", async () => {
    const page = vi.fn(
      async (): Promise<MessagesPageResult> => ({
        session_id: SESSION,
        max_seq: 900,
        events: [event(899), event(900)],
        messages: [],
        complete: true,
      }),
    );
    const source = createMessagesPageBackfillSource(page);
    const result = await source.pageLatest(SESSION, 500);
    expect(page).toHaveBeenCalledWith({ session_id: SESSION, limit: 500 });
    expect(result.events.map((item) => item.seq)).toEqual([899, 900]);
    expect(result.complete).toBe(true);
  });

  it("核心 readback_gap_too_large 同码透传为 ReadbackGapTooLargeError", async () => {
    const page = vi.fn(async () => {
      throw { code: "readback_gap_too_large", message: "缺口过大" };
    });
    const source = createMessagesPageBackfillSource(page);
    await expect(source.backfill(SESSION, 0)).rejects.toBeInstanceOf(
      ReadbackGapTooLargeError,
    );
    expect(isReadbackGapTooLarge({ code: "readback_gap_too_large" })).toBe(true);
    expect(isReadbackGapTooLarge({ code: "internal" })).toBe(false);
    expect(isReadbackGapTooLarge("boom")).toBe(false);
  });

  it("其他错误原样抛出（不伪装为缺口过大）", async () => {
    const page = vi.fn(async () => {
      throw new Error("transport down");
    });
    const source = createMessagesPageBackfillSource(page);
    await expect(source.backfill(SESSION, 0)).rejects.toThrow("transport down");
  });

  it("productionBackfillSource 绑定注入的 SessionIpc", async () => {
    const messagesPage = vi.fn(
      async (): Promise<MessagesPageResult> => ({
        session_id: SESSION,
        max_seq: 1,
        events: [event(1)],
        complete: true,
      }),
    );
    const ipc = { messagesPage } as unknown as SessionIpc;
    const source = productionBackfillSource(ipc);
    const result = await source.pageLatest(SESSION, 500);
    expect(messagesPage).toHaveBeenCalledWith({ session_id: SESSION, limit: 500 });
    expect(result.events).toHaveLength(1);
  });
});
