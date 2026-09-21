import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { HealthReport } from "./health";
import { useHealthPolling } from "./useHealthPolling";

const report = (state: "normal" | "persist_degraded"): HealthReport => ({
  storage_state: state,
  write_queue_depth: 0,
  runtimes: null,
  ts: 1,
});

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useHealthPolling（M2-07 DoD5）", () => {
  it("每 5s 轮询一次 `health`，正常态为 normal", async () => {
    const fetchHealth = vi.fn().mockResolvedValue(report("normal"));
    const { result } = renderHook(() => useHealthPolling({ fetchHealth }));

    await advance(0);
    expect(result.current.status).toBe("normal");
    expect(fetchHealth).toHaveBeenCalledTimes(1);

    await advance(5_000);
    expect(fetchHealth).toHaveBeenCalledTimes(2);
    await advance(5_000);
    expect(fetchHealth).toHaveBeenCalledTimes(3);
    expect(result.current.status).toBe("normal");
  });

  it("persist_degraded → degraded（含触发源透传）", async () => {
    const fetchHealth = vi.fn().mockResolvedValue({
      ...report("persist_degraded"),
      degrade_trigger: "write_failure",
      detail: "写队列已关闭",
    });
    const { result } = renderHook(() => useHealthPolling({ fetchHealth }));
    await advance(0);
    expect(result.current.status).toBe("degraded");
    expect(result.current.report?.degrade_trigger).toBe("write_failure");
  });

  it("调用挂起 15s → unresponsive；此前保持已有状态", async () => {
    const fetchHealth = vi
      .fn()
      .mockImplementation(() => new Promise<HealthReport>(() => {}));
    const { result } = renderHook(() => useHealthPolling({ fetchHealth }));

    await advance(10_000);
    expect(result.current.status).toBe("loading");
    await advance(4_999);
    expect(result.current.status).toBe("loading");
    await advance(1);
    expect(result.current.status).toBe("unresponsive");
  });

  it("连续失败（第 3 次挂起）同样在 15s 判定；后续成功恢复 normal", async () => {
    let resolvePending: ((value: HealthReport) => void) | null = null;
    const fetchHealth = vi
      .fn()
      .mockRejectedValueOnce({ code: "core_not_ready", message: "未就绪" })
      .mockRejectedValueOnce({ code: "core_not_ready", message: "未就绪" })
      .mockImplementationOnce(
        () =>
          new Promise<HealthReport>((resolve) => {
            resolvePending = resolve;
          }),
      );
    const { result } = renderHook(() => useHealthPolling({ fetchHealth }));

    await advance(10_000);
    expect(result.current.status).toBe("loading");
    expect(result.current.error).toContain("未就绪");

    await advance(5_000);
    expect(result.current.status).toBe("unresponsive");

    await act(async () => {
      resolvePending?.(report("normal"));
      await Promise.resolve();
    });
    expect(result.current.status).toBe("normal");
    expect(result.current.error).toBeNull();
  });

  it("最近一次成功后 15s 无响应才置 unresponsive（从成功时刻起算）", async () => {
    const fetchHealth = vi
      .fn()
      .mockResolvedValueOnce(report("normal"))
      .mockImplementation(() => new Promise<HealthReport>(() => {}));
    const { result } = renderHook(() => useHealthPolling({ fetchHealth }));

    await advance(0);
    expect(result.current.status).toBe("normal");

    await advance(10_000);
    expect(result.current.status).toBe("normal");
    await advance(5_000);
    expect(result.current.status).toBe("unresponsive");
  });

  it("测试注入的轮询/超时参数生效", async () => {
    const fetchHealth = vi.fn().mockResolvedValue(report("normal"));
    const { result } = renderHook(() =>
      useHealthPolling({
        fetchHealth,
        pollIntervalMs: 1_000,
        timeoutMs: 3_000,
      }),
    );
    await advance(0);
    await advance(1_000);
    await advance(1_000);
    expect(fetchHealth).toHaveBeenCalledTimes(3);
    expect(result.current.status).toBe("normal");
  });
});
