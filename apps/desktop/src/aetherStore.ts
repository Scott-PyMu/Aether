/**
 * 应用级 EventStore 单例（M3-01；M3-02 注入生产补读源）。
 *
 * 生产补读源 = `messages_page` IPC（`backfillSource.ts`）：缺口 ≤10k 断点续传；
 * `readback_gap_too_large` 同码透传；>10k 时 UI 提示确认后清缓存重载最近 N 条
 * （不重启核心、不重启应用）。测试请自行构造 `EventStore` 注入替身。
 */
import { productionBackfillSource } from "./backfillSource";
import { EventStore } from "./eventStore";

/** 全应用共享的事件流存储（单例）。 */
export const appEventStore = new EventStore({
  backfill: productionBackfillSource(),
});
