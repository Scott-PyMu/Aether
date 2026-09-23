/**
 * 会话事件流存储（EventStore，M3-01；设计 D7 / D8 / D4）。
 *
 * 口径：
 * - **seq 去重 + 乱序排序**：以 `(session_id, seq)` 为键；迟到事件按 seq 插入有序位置；
 *   `evt.id` 全局幂等去重（D4）；
 * - **last_seq 补读**：检测到缺口（收到的 seq > 水位 + 1）且缺口 ≤ {@link READBACK_GAP_LIMIT}
 *   时，经注入的 {@link EventBackfillSource} 断点续传（D4：缺口从 events 表读，上限 10k）；
 * - **缺口 > 10k**：拒绝自动补发，UI 显示「历史消息过多，请关闭并重新打开会话」；
 *   用户确认后清空当前会话缓存，按 `last_seq` 分页加载最近 N 条（默认
 *   {@link DEFAULT_RELOAD_LIMIT}，可配置），**不重启核心、不重启应用**（D4 失败场景表）；
 * - **16ms 批处理**：`ingest` 只入本地队列；每 {@link EVENT_BATCH_INTERVAL_MS} 合并一次
 *   发布（D8 UI 渲染：16ms 合并 flush，上限 60 次/秒）；队列本地有界，慢消费不阻塞上游；
 * - 状态经 zustand vanilla store 暴露（每会话一个 store），React 侧用
 *   {@link useSessionEvents} 订阅。
 *
 * 生产补读通道（`messages_page` IPC）随 M3-02 会话命令装配接线；本模块以可注入
 * 数据源交付（单测/集成），缺口在 `docs/M3-01-证据.md` 登记。
 */
import type { AetherEvent } from "@aether/protocol";
import { createStore, type StoreApi } from "zustand/vanilla";

/** 批处理窗口（D8：16ms 合并 flush，上限 60 次/秒）。 */
export const EVENT_BATCH_INTERVAL_MS = 16;

/** 自动补读上限（D4：缺口 ≤10k 从 events 表读；>10k 拒绝自动补发）。 */
export const READBACK_GAP_LIMIT = 10_000;

/** 缓存清空后重载的默认条数（DoD2：默认 500，可配置）。 */
export const DEFAULT_RELOAD_LIMIT = 500;

/** 本地入站队列上限（慢消费者本地有界保护；仅影响本 UI，不阻塞上游）。 */
export const DEFAULT_MAX_BUFFERED_EVENTS = 100_000;

/** 补读缺口超过 10k：拒绝自动补发（D4 失败场景表；错误码与核心一致）。 */
export class ReadbackGapTooLargeError extends Error {
  readonly code = "readback_gap_too_large";

  constructor(readonly gap?: number) {
    super("补读缺口超过 10k 上限（D4）：拒绝自动补发，请重开会话");
    this.name = "ReadbackGapTooLargeError";
  }
}

/** 补读通道未接线（M3-01 生产缺口；M3-02 会话命令装配后消除）。 */
export class BackfillUnavailableError extends Error {
  readonly code = "backfill_unavailable";

  constructor() {
    super("补读通道未接线（M3-02 会话命令装配）");
    this.name = "BackfillUnavailableError";
  }
}

/** 补读/重载的一页结果。 */
export interface EventPage {
  /** 事件列表（升序或任意顺序，EventStore 内部会再排序）。 */
  events: AetherEvent[];
  /** 是否已到当前最新（`false` 表示可能还有后续缺口）。 */
  complete: boolean;
}

/**
 * 事件补读数据源（生产 = `messages_page` IPC；测试 = 替身）。
 *
 * 契约：`backfill` 以 `lastSeq` 为断点返回其后的补读页；缺口超过 10k 时抛
 * {@link ReadbackGapTooLargeError}（与核心 `readback_gap_too_large` 同码）。
 * `pageLatest` 用于缓存清空后的重载：返回最近 `limit` 条。
 */
export interface EventBackfillSource {
  backfill(sessionId: string, lastSeq: number): Promise<EventPage>;
  pageLatest(sessionId: string, limit: number): Promise<EventPage>;
}

/** 每会话的对外视图状态（zustand store 形状）。 */
export interface SessionEventsState {
  sessionId: string;
  /** 已发布的有序事件（按 seq 升序）。 */
  events: AetherEvent[];
  /** 连续水位（最后一条无缺口事件；补读断点）。 */
  lastSeq: number;
  /** 存在未补齐缺口。 */
  gap: boolean;
  /** 缺口 >10k（需用户确认重开会话/重载最近 N 条）。 */
  historyTooLarge: boolean;
  /** `confirmReload` 进行中。 */
  reloadPending: boolean;
  /** 重载条数（默认 500，可配置）。 */
  reloadLimit: number;
  /** 最近一次补读/重载错误（展示用）。 */
  error: string | null;
}

export interface EventStoreOptions {
  /** 补读数据源；缺省 = 不自动补读（缺口保留在 `gap` 状态）。 */
  backfill?: EventBackfillSource;
  /** 批处理窗口毫秒（默认 {@link EVENT_BATCH_INTERVAL_MS}）。 */
  batchMs?: number;
  /** 缓存清空后重载条数（默认 {@link DEFAULT_RELOAD_LIMIT}）。 */
  reloadLimit?: number;
  /** 自动补读缺口上限（默认 {@link READBACK_GAP_LIMIT}）。 */
  gapLimit?: number;
  /** 本地入站队列上限（默认 {@link DEFAULT_MAX_BUFFERED_EVENTS}）。 */
  maxBufferedEvents?: number;
  /** 诊断：flush 回调（测试/诊断）。 */
  onFlush?: (sessionId: string, flushed: number) => void;
}

interface SessionRuntime {
  sessionId: string;
  store: StoreApi<SessionEventsState>;
  events: AetherEvent[];
  index: Map<number, AetherEvent>;
  seenIds: Set<string>;
  incoming: AetherEvent[];
  pending: Map<number, AetherEvent>;
  flushTimer: ReturnType<typeof setTimeout> | null;
  /** 连续水位（最后一条无缺口事件 seq；补读断点）。 */
  watermark: number;
  /** 存在未补齐缺口/被丢弃事件，需要（或等待）补读。 */
  needsBackfill: boolean;
  /** 是否有被本地队列丢弃的事件（丢弃不可由连续性证明恢复）。 */
  droppedPending: boolean;
  backfillInFlight: boolean;
  backfillRetryQueued: boolean;
  duplicates: number;
  dropped: number;
}

function insertSorted(events: AetherEvent[], event: AetherEvent): void {
  let low = 0;
  let high = events.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if ((events[middle]?.seq ?? 0) < event.seq) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  events.splice(low, 0, event);
}

function describeError(error: unknown): string {
  if (error instanceof Error && error.message) {
    return error.message;
  }
  return String(error);
}

/**
 * 事件流存储（每会话独立视图；批处理 + 缺口补读状态机）。
 *
 * 线程模型（WebView 单线程）：`ingest` 仅入队，不做同步重活；补读为异步，
 * 不阻塞事件回调（慢消费不触发全局背压，DoD4）。
 */
export class EventStore {
  private readonly sessions = new Map<string, SessionRuntime>();
  private readonly options: Required<
    Pick<EventStoreOptions, "batchMs" | "reloadLimit" | "gapLimit" | "maxBufferedEvents">
  > &
    Pick<EventStoreOptions, "backfill" | "onFlush">;

  constructor(options: EventStoreOptions = {}) {
    this.options = {
      batchMs: options.batchMs ?? EVENT_BATCH_INTERVAL_MS,
      reloadLimit: options.reloadLimit ?? DEFAULT_RELOAD_LIMIT,
      gapLimit: options.gapLimit ?? READBACK_GAP_LIMIT,
      maxBufferedEvents: options.maxBufferedEvents ?? DEFAULT_MAX_BUFFERED_EVENTS,
      backfill: options.backfill,
      onFlush: options.onFlush,
    };
  }

  /** 会话视图 store（惰性创建；React 侧经 `useStore` 订阅）。 */
  session(sessionId: string): StoreApi<SessionEventsState> {
    return this.runtime(sessionId).store;
  }

  /** 会话状态快照（测试/非 React 代码）。 */
  getState(sessionId: string): SessionEventsState {
    return this.runtime(sessionId).store.getState();
  }

  /** 已登记会话 id（诊断）。 */
  sessionIds(): string[] {
    return [...this.sessions.keys()];
  }

  /**
   * 摄入一条事件（非阻塞）：按会话入队，16ms 批处理后发布。
   *
   * 去重（`evt.id` / `seq`）与缺口检测在批处理阶段完成；队列超限时丢弃并置
   * `gap`（本地有界，不阻塞上游；缺口由补读恢复）。
   */
  ingest(event: AetherEvent): void {
    const runtime = this.runtime(event.session_id);
    if (runtime.incoming.length >= this.options.maxBufferedEvents) {
      runtime.dropped += 1;
      runtime.needsBackfill = true;
      runtime.droppedPending = true;
      this.setGap(runtime, true);
      return;
    }
    runtime.incoming.push(event);
    this.scheduleFlush(runtime);
  }

  /** 立即处理全部待发布队列（测试与定时器；生产由 16ms 定时器驱动）。 */
  flush(): void {
    for (const runtime of this.sessions.values()) {
      this.flushSession(runtime);
    }
  }

  /**
   * 用户确认「历史消息过多」：清空当前会话缓存，按 `last_seq` 分页加载最近 N 条。
   *
   * 不重启核心、不重启应用（DoD2）；成功后 `gap=false`、`historyTooLarge=false`，
   * 水位重置为重载页最大 seq。
   */
  async confirmReload(sessionId: string): Promise<void> {
    const runtime = this.runtime(sessionId);
    runtime.store.setState({ reloadPending: true, error: null });
    const source = this.options.backfill;
    if (!source) {
      runtime.store.setState({
        reloadPending: false,
        error: new BackfillUnavailableError().message,
      });
      return;
    }
    try {
      const page = await source.pageLatest(sessionId, this.options.reloadLimit);
      this.loadBaseline(runtime, page.events);
    } catch (error) {
      runtime.store.setState({ reloadPending: false, error: describeError(error) });
    }
  }

  /** 终止所有定时器（应用卸载/测试清理）。 */
  dispose(): void {
    for (const runtime of this.sessions.values()) {
      if (runtime.flushTimer !== null) {
        clearTimeout(runtime.flushTimer);
        runtime.flushTimer = null;
      }
    }
    this.sessions.clear();
  }

  private runtime(sessionId: string): SessionRuntime {
    const existing = this.sessions.get(sessionId);
    if (existing) {
      return existing;
    }
    const store = createStore<SessionEventsState>(() => ({
      sessionId,
      events: [],
      lastSeq: 0,
      gap: false,
      historyTooLarge: false,
      reloadPending: false,
      reloadLimit: this.options.reloadLimit,
      error: null,
    }));
    const runtime: SessionRuntime = {
      sessionId,
      store,
      events: [],
      index: new Map(),
      seenIds: new Set(),
      incoming: [],
      pending: new Map(),
      flushTimer: null,
      watermark: 0,
      needsBackfill: false,
      droppedPending: false,
      backfillInFlight: false,
      backfillRetryQueued: false,
      duplicates: 0,
      dropped: 0,
    };
    this.sessions.set(sessionId, runtime);
    return runtime;
  }

  private scheduleFlush(runtime: SessionRuntime): void {
    if (runtime.flushTimer !== null) {
      return;
    }
    runtime.flushTimer = setTimeout(() => {
      runtime.flushTimer = null;
      this.flushSession(runtime);
    }, this.options.batchMs);
  }

  private flushSession(runtime: SessionRuntime): void {
    if (runtime.flushTimer !== null) {
      clearTimeout(runtime.flushTimer);
      runtime.flushTimer = null;
    }
    const queued = runtime.incoming;
    if (queued.length === 0) {
      return;
    }
    runtime.incoming = [];
    for (const event of queued) {
      this.applyEvent(runtime, event);
    }
    this.publish(runtime);
    this.options.onFlush?.(runtime.sessionId, queued.length);
    void this.maybeBackfill(runtime);
  }

  private applyEvent(runtime: SessionRuntime, event: AetherEvent): void {
    if (runtime.seenIds.has(event.id) || runtime.index.has(event.seq)) {
      runtime.duplicates += 1;
      return;
    }
    if (event.seq <= runtime.watermark) {
      runtime.seenIds.add(event.id);
      runtime.index.set(event.seq, event);
      insertSorted(runtime.events, event);
      return;
    }
    if (event.seq === runtime.watermark + 1) {
      runtime.seenIds.add(event.id);
      runtime.index.set(event.seq, event);
      runtime.events.push(event);
      runtime.watermark = event.seq;
      let next = runtime.watermark + 1;
      while (runtime.pending.has(next)) {
        const pending = runtime.pending.get(next);
        runtime.pending.delete(next);
        if (pending) {
          runtime.seenIds.add(pending.id);
          runtime.index.set(pending.seq, pending);
          runtime.events.push(pending);
          runtime.watermark = pending.seq;
        }
        next += 1;
      }
      if (runtime.pending.size === 0 && !runtime.droppedPending) {
        this.resolveGap(runtime);
      }
      return;
    }
    // 缺口：seq > 水位 + 1。
    runtime.pending.set(event.seq, event);
    runtime.needsBackfill = true;
    const gap = event.seq - runtime.watermark;
    if (gap > this.options.gapLimit) {
      this.markHistoryTooLarge(runtime);
    } else {
      this.setGap(runtime, true);
    }
  }

  private publish(runtime: SessionRuntime): void {
    const state = runtime.store.getState();
    if (
      runtime.events.length === state.events.length &&
      runtime.watermark === state.lastSeq
    ) {
      return;
    }
    runtime.store.setState({
      events: [...runtime.events],
      lastSeq: runtime.watermark,
    });
  }

  private resolveGap(runtime: SessionRuntime): void {
    runtime.needsBackfill = false;
    runtime.droppedPending = false;
    if (runtime.store.getState().gap) {
      runtime.store.setState({ gap: false });
    }
  }

  private setGap(runtime: SessionRuntime, gap: boolean): void {
    if (runtime.store.getState().gap === gap) {
      return;
    }
    runtime.store.setState({ gap });
  }

  private markHistoryTooLarge(runtime: SessionRuntime): void {
    const state = runtime.store.getState();
    if (state.historyTooLarge) {
      return;
    }
    runtime.store.setState({ historyTooLarge: true, gap: true, error: null });
  }

  private async maybeBackfill(runtime: SessionRuntime): Promise<void> {
    const source = this.options.backfill;
    const state = runtime.store.getState();
    if (!source || state.historyTooLarge || runtime.backfillInFlight) {
      return;
    }
    if (!runtime.needsBackfill) {
      return;
    }
    runtime.backfillInFlight = true;
    try {
      const page = await source.backfill(runtime.sessionId, runtime.watermark);
      for (const event of page.events) {
        this.applyEvent(runtime, event);
      }
      this.publish(runtime);
      if (runtime.pending.size === 0 && (page.complete || !runtime.droppedPending)) {
        this.resolveGap(runtime);
      } else if (!page.complete && page.events.length > 0) {
        // 单页不足且确有进展：排一次续读（避免空页死循环）。
        runtime.backfillRetryQueued = true;
      }
    } catch (error) {
      if (error instanceof ReadbackGapTooLargeError) {
        this.markHistoryTooLarge(runtime);
      } else {
        runtime.store.setState({ error: describeError(error) });
      }
    } finally {
      runtime.backfillInFlight = false;
      if (runtime.backfillRetryQueued && runtime.needsBackfill) {
        runtime.backfillRetryQueued = false;
        void this.maybeBackfill(runtime);
      } else {
        runtime.backfillRetryQueued = false;
      }
    }
  }

  /** 清空缓存并按重载页建立新基线（DoD2：不重启核心/应用）。 */
  private loadBaseline(runtime: SessionRuntime, events: AetherEvent[]): void {
    const ordered = [...events].sort((a, b) => a.seq - b.seq);
    const deduped: AetherEvent[] = [];
    for (const event of ordered) {
      const last = deduped[deduped.length - 1];
      if (!last || last.seq !== event.seq) {
        deduped.push(event);
      }
    }
    if (runtime.flushTimer !== null) {
      clearTimeout(runtime.flushTimer);
      runtime.flushTimer = null;
    }
    runtime.events = deduped;
    runtime.index = new Map(deduped.map((event) => [event.seq, event]));
    runtime.seenIds = new Set(deduped.map((event) => event.id));
    runtime.pending.clear();
    runtime.incoming = [];
    runtime.watermark = deduped.length > 0 ? (deduped[deduped.length - 1]?.seq ?? 0) : 0;
    runtime.needsBackfill = false;
    runtime.store.setState({
      events: [...deduped],
      lastSeq: runtime.watermark,
      gap: false,
      historyTooLarge: false,
      reloadPending: false,
      error: null,
    });
  }
}
