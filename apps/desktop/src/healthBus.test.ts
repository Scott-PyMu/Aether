/**
 * 健康共享总线单测（M3-06）：发布/订阅、快照稳定、测试复位。
 */
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  healthBusSnapshot,
  publishHealthState,
  resetHealthBusForTests,
  subscribeHealthBus,
} from "./healthBus";

afterEach(() => {
  resetHealthBusForTests();
  vi.restoreAllMocks();
});

describe("healthBus（M3-06）", () => {
  it("初始为 loading；发布后订阅者收到通知且快照更新", () => {
    expect(healthBusSnapshot().status).toBe("loading");
    const listener = vi.fn();
    const unsubscribe = subscribeHealthBus(listener);

    publishHealthState({
      status: "degraded",
      report: {
        storage_state: "persist_degraded",
        write_queue_depth: 3,
        runtimes: null,
        ts: 1,
        degrade_trigger: "write_failure",
      },
      error: null,
    });
    expect(listener).toHaveBeenCalledTimes(1);
    expect(healthBusSnapshot().status).toBe("degraded");
    expect(healthBusSnapshot().report?.storage_state).toBe("persist_degraded");

    unsubscribe();
    publishHealthState({ status: "normal", report: null, error: null });
    expect(listener).toHaveBeenCalledTimes(1);
    expect(healthBusSnapshot().status).toBe("normal");
  });

  it("复位清空订阅（测试隔离）", () => {
    const listener = vi.fn();
    subscribeHealthBus(listener);
    resetHealthBusForTests();
    publishHealthState({ status: "unresponsive", report: null, error: "timeout" });
    expect(listener).not.toHaveBeenCalled();
    expect(healthBusSnapshot().status).toBe("unresponsive");
  });
});
