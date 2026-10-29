/**
 * M9-09 崩溃恢复策略单测（退避 1/2/4/8/16/30s；60s ≥5 次熔断）。
 */
import { describe, expect, it } from "vitest";

import {
  CRASH_WINDOW_MS,
  DaemonRestartPolicy,
  MAX_STOPS_IN_WINDOW,
  RESTART_BACKOFF_MS,
  restartDelayForAttempt,
} from "./daemonLifecycle";

describe("DaemonRestartPolicy", () => {
  it("退避序列固定为 1/2/4/8/16/30s，超出沿用 30s", () => {
    expect(RESTART_BACKOFF_MS).toEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000]);
    expect([0, 1, 2, 3, 4, 5, 6].map(restartDelayForAttempt)).toEqual([
      1_000,
      2_000,
      4_000,
      8_000,
      16_000,
      30_000,
      30_000,
    ]);
  });

  it("前 4 次停止按 1/2/4/8s 退避", () => {
    const policy = new DaemonRestartPolicy();
    const delays = [0, 1, 2, 3].map((index) => policy.recordStop(index * 100).delayMs);
    expect(delays).toEqual([1_000, 2_000, 4_000, 8_000]);
  });

  it("60s 内第 5 次停止触发熔断（不再自动重启）", () => {
    const policy = new DaemonRestartPolicy();
    const decisions = [0, 1, 2, 3, 4].map((index) => policy.recordStop(index * 1_000));
    expect(decisions.slice(0, 4).every((decision) => decision.restart)).toBe(true);
    expect(decisions[4]).toMatchObject({
      restart: false,
      paused: true,
      stopsInWindow: MAX_STOPS_IN_WINDOW,
    });
    expect(policy.isPaused).toBe(true);
  });

  it("窗口外的停止被剔除，不触发熔断", () => {
    const policy = new DaemonRestartPolicy();
    for (let index = 0; index < 4; index += 1) {
      policy.recordStop(index * 1_000);
    }
    const later = CRASH_WINDOW_MS + 10_000;
    const decision = policy.recordStop(later);
    expect(decision).toMatchObject({ restart: true, stopsInWindow: 1 });
    expect(policy.isPaused).toBe(false);
  });

  it("握手成功不立即重置退避；稳定 60s 后才回到 1s", () => {
    const policy = new DaemonRestartPolicy();
    policy.recordStop(0);
    policy.markReady(100);
    expect(policy.recordStop(500).delayMs).toBe(2_000);
    policy.markReady(600);
    // 距上次 ready 不足 60s：继续 4s。
    expect(policy.recordStop(1_000).delayMs).toBe(4_000);
    policy.markReady(10_000);
    // 稳定满 60s：退避重置；窗口内旧停止也被剔除。
    expect(policy.recordStop(10_000 + CRASH_WINDOW_MS + 1_000).delayMs).toBe(1_000);
  });

  it("60s 内第 5 次停止触发熔断（不稳定重启不会重置退避）", () => {
    const policy = new DaemonRestartPolicy();
    const decisions = [];
    for (let index = 0; index < 5; index += 1) {
      const at = index * 5_000;
      policy.markReady(at + 1);
      decisions.push(policy.recordStop(at));
    }
    expect(decisions.slice(0, 4).map((decision) => decision.delayMs)).toEqual([
      1_000,
      2_000,
      4_000,
      8_000,
    ]);
    expect(decisions[4]).toMatchObject({
      restart: false,
      paused: true,
      stopsInWindow: MAX_STOPS_IN_WINDOW,
    });
  });

  it("手动重试清空熔断与窗口", () => {
    const policy = new DaemonRestartPolicy();
    for (let index = 0; index < 5; index += 1) {
      policy.recordStop(index);
    }
    expect(policy.isPaused).toBe(true);
    policy.manualRetry();
    expect(policy.snapshot(5).paused).toBe(false);
    expect(policy.recordStop(6)).toMatchObject({ restart: true, stopsInWindow: 1 });
  });

  it("快照记录最近一次停止（code/signal）", () => {
    const policy = new DaemonRestartPolicy();
    policy.recordStop(1_000, null, "SIGKILL");
    const snapshot = policy.snapshot(1_000);
    expect(snapshot.lastStop).toEqual({ at: 1_000, code: null, signal: "SIGKILL" });
    expect(snapshot.totalStops).toBe(1);
  });
});
