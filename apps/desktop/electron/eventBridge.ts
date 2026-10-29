/**
 * Electron main 事件桥（M9-05 DoD-4；ADR-029 D-029-5 / §4.4 / §4.7）。
 *
 * 链路：daemon stdout `aether.event` 通知 → `MainEventBridge.push` →
 * 单通道 FIFO + 16ms 批处理 → `webContents.send('aether:event', event)`。
 *
 * 口径：
 * - **批内顺序不变**（单队列 FIFO；16ms 仅合并发送时机）；
 * - 有界队列：满时 `message.delta` / `usage` / `log` 可丢并计数；其余按控制事件；
 * - 控制事件不可丢：先丢队列中的 delta 让路；仍满则 flush 后触发 journal 补读
 *   （`readback`，main 以 `messages_page` 的 `last_seq` 续读）；
 * - **连续 3 次补读失败**才升级：审计 + 重建连接（`rebuildConnection`）；不重启
 *   daemon（daemon 进程由 M9-09 生命周期管理）；
 * - `readback_gap_too_large`（>10k）不自动重试；renderer 继续用既有
 *   「历史消息过多，请关闭并重新打开会话」路径；
 * - renderer 的 15s 无 `health` 判定沿用 `apps/desktop/src/health.ts`
 *   （`HEALTH_TIMEOUT_MS = 15_000`），本模块只负责事件通道。
 */
import type { AetherEvent } from "@aether/protocol";

/** Electron IPC 通道（ADR-029 §4.4 三处命名映射之一）。 */
export const EVENT_IPC_CHANNEL = "aether:event";
/** main → renderer 批处理窗口（D8：16ms；批内顺序不变）。 */
export const EVENT_BATCH_INTERVAL_MS = 16;
/** main 侧有界队列容量（ADR-029 §4.7 建议 4096）。 */
export const EVENT_QUEUE_CAPACITY = 4_096;
/** 连续补读失败升级阈值（ADR-029 §4.7：连续 3 次）。 */
export const READBACK_MAX_ATTEMPTS = 3;
/** 单次补读最多页数（10k 缺口 / 500 条 ≈ 20；留余量）。 */
export const READBACK_MAX_PAGES = 32;
/** 补读缺口过大错误码（D4；不自动重试）。 */
export const READBACK_GAP_TOO_LARGE = "readback_gap_too_large";

const DROPPABLE_EVENT_TYPES: ReadonlySet<string> = new Set([
  "message.delta",
  "usage",
  "log",
]);

/** 事件是否可丢（ADR-029 §4.7 原文；其余均按控制事件处理）。 */
export function isDroppableEvent(eventType: string): boolean {
  return DROPPABLE_EVENT_TYPES.has(eventType);
}

/** 补读一页（与 `messages_page` 的 events 补读流同构）。 */
export interface EventPage {
  events: AetherEvent[];
  complete: boolean;
}

/** 补读通道（生产 = `messages_page`；测试注入）。 */
export type EventReadback = (
  sessionId: string,
  lastSeq: number,
) => Promise<EventPage>;

/** 升级审计记录（main 转写 `shell.log`；daemon 侧审计见 M9-06）。 */
export interface EventBridgeAudit {
  action: "event.bridge.escalated";
  sessionId: string;
  lastSeq: number;
  attempts: number;
  detail: string;
}

export type EventSender = (channel: string, event: AetherEvent) => void;

export interface EventBridgeOptions {
  /** 发送到 renderer（生产 = `webContents.send`）。 */
  send: EventSender;
  /** journal 补读（生产 = daemon `messages_page`；缺省不补读）。 */
  readback?: EventReadback;
  /** 升级审计出口（缺省仅计数）。 */
  audit?: (record: EventBridgeAudit) => void;
  /** 重建连接（main → daemon 客户端；缺省仅计数）。 */
  rebuildConnection?: (sessionId: string) => void;
  /** 定时器注入（测试；默认 `setTimeout`）。 */
  setTimer?: (callback: () => void, ms: number) => ReturnType<typeof setTimeout>;
  /** 定时器清理注入（测试；默认 `clearTimeout`）。 */
  clearTimer?: (handle: ReturnType<typeof setTimeout>) => void;
  /** 有界队列容量（默认 {@link EVENT_QUEUE_CAPACITY}）。 */
  queueCapacity?: number;
  /** 批处理窗口（默认 {@link EVENT_BATCH_INTERVAL_MS}）。 */
  batchIntervalMs?: number;
  /** 连续补读失败阈值（默认 {@link READBACK_MAX_ATTEMPTS}）。 */
  readbackMaxAttempts?: number;
}

/** 诊断快照（证据/日志）。 */
export interface EventBridgeSnapshot {
  queued: number;
  forwarded: number;
  droppedDelta: number;
  readbackEvents: number;
  readbackFailures: number;
  connectionRebuilds: number;
  readbackGapTooLarge: number;
}

/**
 * main 单通道事件桥（无 Electron 依赖；便于单测）。
 */
export class MainEventBridge {
  private readonly sendEvent: EventSender;
  private readonly readback?: EventReadback;
  private readonly audit?: (record: EventBridgeAudit) => void;
  private readonly rebuildConnection?: (sessionId: string) => void;
  private readonly setTimer: (
    callback: () => void,
    ms: number,
  ) => ReturnType<typeof setTimeout>;
  private readonly clearTimer: (handle: ReturnType<typeof setTimeout>) => void;
  private readonly queueCapacity: number;
  private readonly batchIntervalMs: number;
  private readonly readbackMaxAttempts: number;

  private queue: AetherEvent[] = [];
  private timer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private readonly lastSeq = new Map<string, number>();
  private readonly readbackInFlight = new Set<string>();
  private readonly readbackQueued = new Set<string>();
  private consecutiveReadbackFailures = 0;
  private forwarded = 0;
  private droppedDelta = 0;
  private readbackEvents = 0;
  private readbackFailures = 0;
  private connectionRebuilds = 0;
  private readbackGapTooLarge = 0;

  constructor(options: EventBridgeOptions) {
    this.sendEvent = options.send;
    this.readback = options.readback;
    this.audit = options.audit;
    this.rebuildConnection = options.rebuildConnection;
    this.setTimer = options.setTimer ?? setTimeout;
    this.clearTimer = options.clearTimer ?? clearTimeout;
    const queueCapacity = options.queueCapacity ?? EVENT_QUEUE_CAPACITY;
    const batchIntervalMs = options.batchIntervalMs ?? EVENT_BATCH_INTERVAL_MS;
    const readbackMaxAttempts =
      options.readbackMaxAttempts ?? READBACK_MAX_ATTEMPTS;
    if (queueCapacity <= 0) {
      throw new Error("queueCapacity 必须 >0");
    }
    if (batchIntervalMs <= 0) {
      throw new Error("batchIntervalMs 必须 >0");
    }
    if (readbackMaxAttempts <= 0) {
      throw new Error("readbackMaxAttempts 必须 >0");
    }
    this.queueCapacity = queueCapacity;
    this.batchIntervalMs = batchIntervalMs;
    this.readbackMaxAttempts = readbackMaxAttempts;
  }

  /** daemon `aether.event` 通知入口（严格按到达顺序入队）。 */
  push(event: AetherEvent): void {
    if (this.disposed) {
      return;
    }
    if (this.queue.length >= this.queueCapacity) {
      if (isDroppableEvent(event.type)) {
        this.droppedDelta += 1;
        return;
      }
      // 控制事件不可丢：先丢队列中的可丢事件让路（ADR-029 §4.7）。
      const kept: AetherEvent[] = [];
      let dropped = 0;
      for (const queued of this.queue) {
        if (isDroppableEvent(queued.type)) {
          dropped += 1;
        } else {
          kept.push(queued);
        }
      }
      this.queue = kept;
      this.droppedDelta += dropped;
      if (this.queue.length >= this.queueCapacity) {
        // 队列全为控制事件：先按 FIFO flush，再触发 journal 补读兜底。
        this.flush();
        this.scheduleReadback(event.session_id);
      }
    }
    this.queue.push(event);
    this.scheduleFlush();
  }

  /** 立即按 FIFO 发送当前队列（批处理定时器取消）。 */
  flush(): void {
    if (this.timer !== null) {
      this.clearTimer(this.timer);
      this.timer = null;
    }
    if (this.queue.length === 0) {
      return;
    }
    const batch = this.queue;
    this.queue = [];
    for (const event of batch) {
      this.sendEvent(EVENT_IPC_CHANNEL, event);
      const previous = this.lastSeq.get(event.session_id) ?? 0;
      if (event.seq > previous) {
        this.lastSeq.set(event.session_id, event.seq);
      }
      this.forwarded += 1;
    }
  }

  /** 已送达 renderer 的会话水位（UI `last_seq` 补读断点）。 */
  lastSeqOf(sessionId: string): number {
    return this.lastSeq.get(sessionId) ?? 0;
  }

  snapshot(): EventBridgeSnapshot {
    return {
      queued: this.queue.length,
      forwarded: this.forwarded,
      droppedDelta: this.droppedDelta,
      readbackEvents: this.readbackEvents,
      readbackFailures: this.readbackFailures,
      connectionRebuilds: this.connectionRebuilds,
      readbackGapTooLarge: this.readbackGapTooLarge,
    };
  }

  /** 停止批处理并丢弃本地队列（renderer reload/退出；不补发）。 */
  dispose(): void {
    this.disposed = true;
    if (this.timer !== null) {
      this.clearTimer(this.timer);
      this.timer = null;
    }
    this.queue = [];
    this.readbackQueued.clear();
  }

  private scheduleFlush(): void {
    if (this.timer !== null) {
      return;
    }
    this.timer = this.setTimer(() => {
      this.timer = null;
      this.flush();
    }, this.batchIntervalMs);
  }

  private enqueueRecovered(event: AetherEvent): void {
    if (this.disposed) {
      return;
    }
    // 补读与在途队列可能重叠（控制事件触发 flush 后仍可能有同 seq 事件在队列）。
    const duplicate = this.queue.some(
      (queued) =>
        queued.session_id === event.session_id && queued.seq === event.seq,
    );
    if (duplicate) {
      return;
    }
    if (this.queue.length >= this.queueCapacity) {
      this.flush();
    }
    this.queue.push(event);
    this.scheduleFlush();
  }

  private scheduleReadback(sessionId: string): void {
    if (!this.readback || this.disposed) {
      return;
    }
    if (this.readbackInFlight.has(sessionId)) {
      // 补读进行中再次溢出：登记一次，避免丢失补读触发（连续失败仍按序累计）。
      this.readbackQueued.add(sessionId);
      return;
    }
    this.readbackInFlight.add(sessionId);
    void this.runReadback(sessionId).finally(() => {
      this.readbackInFlight.delete(sessionId);
      if (this.readbackQueued.delete(sessionId)) {
        this.scheduleReadback(sessionId);
      }
    });
  }

  private async runReadback(sessionId: string): Promise<void> {
    let from = this.lastSeqOf(sessionId);
    for (let page = 0; page < READBACK_MAX_PAGES; page += 1) {
      if (this.disposed) {
        return;
      }
      let result: EventPage;
      try {
        result = await this.readback!(sessionId, from);
      } catch (error) {
        this.handleReadbackFailure(sessionId, from, error);
        return;
      }
      this.consecutiveReadbackFailures = 0;
      this.readbackEvents += result.events.length;
      for (const event of result.events) {
        if (event.session_id !== sessionId) {
          continue;
        }
        if (event.seq <= this.lastSeqOf(sessionId)) {
          continue;
        }
        this.enqueueRecovered(event);
      }
      if (result.complete) {
        return;
      }
      const next = result.events[result.events.length - 1]?.seq ?? 0;
      if (next <= from) {
        return;
      }
      from = next;
    }
  }

  private handleReadbackFailure(
    sessionId: string,
    lastSeq: number,
    error: unknown,
  ): void {
    if (isReadbackGapTooLarge(error)) {
      this.readbackGapTooLarge += 1;
      return;
    }
    this.readbackFailures += 1;
    this.consecutiveReadbackFailures += 1;
    if (this.consecutiveReadbackFailures < this.readbackMaxAttempts) {
      return;
    }
    this.connectionRebuilds += 1;
    this.consecutiveReadbackFailures = 0;
    this.audit?.({
      action: "event.bridge.escalated",
      sessionId,
      lastSeq,
      attempts: this.readbackMaxAttempts,
      detail: `连续 ${this.readbackMaxAttempts} 次补读失败：重建连接（不重启 daemon）`,
    });
    this.rebuildConnection?.(sessionId);
  }
}

function isReadbackGapTooLarge(error: unknown): boolean {
  if (typeof error !== "object" || error === null) {
    return false;
  }
  const record = error as Record<string, unknown>;
  return record.code === READBACK_GAP_TOO_LARGE;
}
