/**
 * 生产补读源（M3-02 属主承接项）：`messages_page` IPC → {@link EventBackfillSource}。
 *
 * 契约：
 * - `backfill(sessionId, lastSeq)`：按 `last_seq` 断点续传（补读页不附带消息历史）；
 * - `pageLatest(sessionId, limit)`：缓存清空后的最近一页（默认 500，可配置）；
 * - 核心返回 `readback_gap_too_large`（缺口 >10k，D4）时抛
 *   {@link ReadbackGapTooLargeError}（**同码透传**，EventStore 转 `historyTooLarge`）。
 */
import type { EventBackfillSource, EventPage } from "./eventStore";
import { ReadbackGapTooLargeError } from "./eventStore";
import {
  sessionIpc,
  type MessagesPageInput,
  type MessagesPageResult,
  type SessionIpc,
} from "./session";

/** 补读单页条数（D7 分页上限 500）。 */
export const BACKFILL_PAGE_LIMIT = 500;

/** 判断 IPC 错误是否为补读缺口过大（同码透传）。 */
export function isReadbackGapTooLarge(error: unknown): boolean {
  if (typeof error !== "object" || error === null) {
    return false;
  }
  const record = error as Record<string, unknown>;
  return record.code === "readback_gap_too_large";
}

/** `messages_page` 调用签名（测试可注入）。 */
export type MessagesPageFn = (input: MessagesPageInput) => Promise<MessagesPageResult>;

/** 构造生产补读源（默认走 Tauri IPC；测试注入替身）。 */
export function createMessagesPageBackfillSource(
  page: MessagesPageFn = (input) => sessionIpc.messagesPage(input),
): EventBackfillSource {
  return {
    async backfill(sessionId: string, lastSeq: number): Promise<EventPage> {
      try {
        const result = await page({
          session_id: sessionId,
          last_seq: lastSeq,
          limit: BACKFILL_PAGE_LIMIT,
        });
        return { events: result.events, complete: result.complete };
      } catch (error) {
        if (isReadbackGapTooLarge(error)) {
          throw new ReadbackGapTooLargeError();
        }
        throw error;
      }
    },
    async pageLatest(sessionId: string, limit: number): Promise<EventPage> {
      const result = await page({ session_id: sessionId, limit });
      return { events: result.events, complete: true };
    },
  };
}

/** 生产补读源单例（`aetherStore` 注入；测试请自行构造）。 */
export function productionBackfillSource(ipc: SessionIpc = sessionIpc): EventBackfillSource {
  return createMessagesPageBackfillSource((input) => ipc.messagesPage(input));
}
