/**
 * daemon 崩溃恢复策略（M9-09 DoD-3；ADR-029 §3.7）。
 *
 * 冻结口径：
 * - daemon 在 `ready` 之后退出 → main 保留窗口、标记 `core_not_ready`，按
 *   **1/2/4/8/16/30s** 退避重启；
 * - **60s 内 ≥5 次停止** → 停止自动重启，等待用户手动重试（S-09 的
 *   `app_restart` 或壳层重试入口）；
 * - 控制投递队列满（journal 补读失败）**不重启 daemon**（由事件桥自理）。
 *
 * 本模块只做纯策略，不依赖 Electron/计时器，便于单测与证据复跑。
 */

/** 退避序列（毫秒；第 7 次及以后沿用最后一项 30s）。 */
export const RESTART_BACKOFF_MS = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000] as const;
/** 第 N 次退避（0-based；超出序列沿用 30s）。 */
export function restartDelayForAttempt(attempt: number): number {
  const index = Math.min(Math.max(0, attempt), RESTART_BACKOFF_MS.length - 1);
  return RESTART_BACKOFF_MS[index] ?? 30_000;
}
/** 崩溃熔断窗口（ADR-029 §3.7：60s）。 */
export const CRASH_WINDOW_MS = 60_000;
/** 窗口内停止次数上限（≥5 → 停止自动重启）。 */
export const MAX_STOPS_IN_WINDOW = 5;

export interface DaemonStopRecord {
  at: number;
  code: number | null;
  signal: NodeJS.Signals | null;
}

export interface RestartDecision {
  /** 是否允许安排自动重启。 */
  restart: boolean;
  /** 本次退避（毫秒）；`restart=false` 时为 0。 */
  delayMs: number;
  /** 当前退避下标（0-based）。 */
  attempt: number;
  /** 60s 窗口内停止次数（含本次）。 */
  stopsInWindow: number;
  /** 是否触发熔断（停止自动重启）。 */
  paused: boolean;
}

export interface DaemonLifecycleSnapshot {
  /** 下一次自动重启的退避下标。 */
  attempt: number;
  /** 是否已熔断（等待手动重试）。 */
  paused: boolean;
  /** 60s 窗口内停止次数。 */
  stopsInWindow: number;
  /** 累计停止次数（进程生命周期内）。 */
  totalStops: number;
  /** 最近一次停止。 */
  lastStop: DaemonStopRecord | null;
}

/**
 * 崩溃恢复策略状态机。
 *
 * 使用方式：
 * 1. daemon `ready` 后每次退出调用 {@link recordStop}；
 * 2. 按 `decision.delayMs` 安排重启；成功握手后调用 {@link markReady}（重置退避
 *    下标，但保留 60s 窗口内的停止历史——崩溃循环仍会触发熔断）；
 * 3. 用户手动重试调用 {@link manualRetry}（清空窗口与退避）。
 */
export class DaemonRestartPolicy {
  private stops: number[] = [];
  private history: DaemonStopRecord[] = [];
  private attempt = 0;
  private paused = false;
  private lastReadyAt: number | null = null;

  /** 记录一次 daemon 停止并给出重启决策。 */
  recordStop(now: number, code: number | null = null, signal: NodeJS.Signals | null = null): RestartDecision {
    this.stops.push(now);
    this.history.push({ at: now, code, signal });
    this.prune(now);
    // 只有稳定运行满 60s 才把退避重置到 1s；「握手成功后立刻崩溃」必须继续
    // 递增退避（否则 crash loop 会每 1s 重启，违反 ADR-029 §3.7）。
    if (this.lastReadyAt !== null && now - this.lastReadyAt >= CRASH_WINDOW_MS) {
      this.attempt = 0;
      this.lastReadyAt = null;
    }

    if (this.stops.length >= MAX_STOPS_IN_WINDOW) {
      this.paused = true;
      return {
        restart: false,
        delayMs: 0,
        attempt: this.attempt,
        stopsInWindow: this.stops.length,
        paused: true,
      };
    }

    const index = Math.min(this.attempt, RESTART_BACKOFF_MS.length - 1);
    const delayMs = restartDelayForAttempt(index);
    this.attempt += 1;
    return {
      restart: true,
      delayMs,
      attempt: index,
      stopsInWindow: this.stops.length,
      paused: false,
    };
  }

  /** 重启成功（握手完成）：记录就绪时间；稳定 60s 后才重置退避。 */
  markReady(now: number = Date.now()): void {
    this.lastReadyAt = now;
  }

  /** 手动重试：清空熔断与窗口（用户显式动作）。 */
  manualRetry(): void {
    this.stops = [];
    this.attempt = 0;
    this.paused = false;
    this.lastReadyAt = null;
  }

  /** 当前快照（E2E/证据）。 */
  snapshot(now: number = Date.now()): DaemonLifecycleSnapshot {
    this.prune(now);
    return {
      attempt: this.attempt,
      paused: this.paused,
      stopsInWindow: this.stops.length,
      totalStops: this.history.length,
      lastStop: this.history[this.history.length - 1] ?? null,
    };
  }

  /** 是否处于熔断。 */
  get isPaused(): boolean {
    return this.paused;
  }

  private prune(now: number): void {
    const cutoff = now - CRASH_WINDOW_MS;
    while (this.stops.length > 0 && (this.stops[0] ?? 0) < cutoff) {
      this.stops.shift();
    }
  }
}
