/**
 * Electron main 事件桥（M9-05 DoD-4）单元测试。
 */
import type { AetherEvent } from "@aether/protocol";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  EVENT_BATCH_INTERVAL_MS,
  EVENT_IPC_CHANNEL,
  MainEventBridge,
  isDroppableEvent,
} from "./eventBridge";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K5A";

function makeEvent(
  seq: number,
  type = "message.delta",
  session = SESSION,
): AetherEvent {
  return {
    v: 1,
    id: `01J${String(seq).padStart(23, "0")}`,
    session_id: session,
    run_id: null,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload: { seq },
  };
}

interface Sent {
  channel: string;
  event: AetherEvent;
}

function recorder(): { sent: Sent[]; send: (channel: string, event: AetherEvent) => void } {
  const sent: Sent[] = [];
  return {
    sent,
    send: (channel, event) => {
      sent.push({ channel, event });
    },
  };
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("MainEventBridge（M9-05 DoD-4）", () => {
  it("16ms 批处理 + 单通道 FIFO（批内顺序不变）", () => {
    vi.useFakeTimers();
    const { sent, send } = recorder();
    const bridge = new MainEventBridge({ send });
    bridge.push(makeEvent(1));
    bridge.push(makeEvent(2, "usage"));
    bridge.push(makeEvent(3, "log"));
    expect(sent).toHaveLength(0);

    vi.advanceTimersByTime(EVENT_BATCH_INTERVAL_MS - 1);
    expect(sent).toHaveLength(0);
    vi.advanceTimersByTime(1);
    expect(sent.map((entry) => entry.event.seq)).toEqual([1, 2, 3]);
    for (const entry of sent) {
      expect(entry.channel).toBe(EVENT_IPC_CHANNEL);
    }
    expect(bridge.snapshot().forwarded).toBe(3);
    bridge.dispose();
  });

  it("可丢集合恰为 message.delta / usage / log（其余按控制事件）", () => {
    expect(isDroppableEvent("message.delta")).toBe(true);
    expect(isDroppableEvent("usage")).toBe(true);
    expect(isDroppableEvent("log")).toBe(true);
    for (const type of [
      "session.created",
      "run.started",
      "permission.requested",
      "runtime.status_changed",
      "error",
    ]) {
      expect(isDroppableEvent(type)).toBe(false);
    }
  });

  it("队列满：delta 丢弃并计数；控制事件仍入队", () => {
    const { sent, send } = recorder();
    const bridge = new MainEventBridge({ send, queueCapacity: 2 });
    bridge.push(makeEvent(1, "session.updated"));
    bridge.push(makeEvent(2, "message.delta"));
    bridge.push(makeEvent(3, "message.delta"));
    expect(bridge.snapshot().droppedDelta).toBe(1);
    bridge.flush();
    expect(sent.map((entry) => entry.event.seq)).toEqual([1, 2]);
    expect(bridge.snapshot().forwarded).toBe(2);
  });

  it("控制事件队列满：丢队列 delta + 触发 journal 补读 + FIFO 发送", async () => {
    const { sent, send } = recorder();
    const readback = vi.fn(async () => ({ events: [], complete: true }));
    const bridge = new MainEventBridge({
      send,
      readback,
      queueCapacity: 2,
    });
    bridge.push(makeEvent(1, "session.updated"));
    bridge.push(makeEvent(2, "message.delta"));
    // 第 3 条控制事件：队列满 → 丢 seq=2 的 delta 让路。
    bridge.push(makeEvent(3, "session.updated"));
    expect(bridge.snapshot().droppedDelta).toBe(1);
    // 第 4 条控制事件：队列全为控制事件 → flush + 补读兜底。
    bridge.push(makeEvent(4, "session.updated"));
    await vi.waitFor(() => {
      expect(readback).toHaveBeenCalledTimes(1);
    });
    expect(readback).toHaveBeenCalledWith(SESSION, 3);
    bridge.flush();
    expect(sent.map((entry) => entry.event.seq)).toEqual([1, 3, 4]);
    expect(bridge.snapshot().connectionRebuilds).toBe(0);
  });

  it("连续 3 次补读失败才审计 + 重建连接（不重启 daemon）", async () => {
    const { send } = recorder();
    const readback = vi.fn(async () => {
      throw new Error("journal readback failed");
    });
    const audit = vi.fn();
    const rebuildConnection = vi.fn();
    const bridge = new MainEventBridge({
      send,
      readback,
      audit,
      rebuildConnection,
      queueCapacity: 1,
    });

    // 三个会话各自触发一次补读（避免同一会话的在途去重；失败计数跨会话累计）。
    for (let attempt = 1; attempt <= 3; attempt += 1) {
      const session = `01J8ZQ5R0N7W9Y8X6V4T2S0K${attempt}`;
      bridge.push(makeEvent(1, "session.updated", session));
      bridge.push(makeEvent(2, "session.updated", session));
      await vi.waitFor(() => {
        expect(readback).toHaveBeenCalledTimes(attempt);
      });
    }
    expect(bridge.snapshot().readbackFailures).toBe(3);
    expect(bridge.snapshot().connectionRebuilds).toBe(1);
    expect(audit).toHaveBeenCalledTimes(1);
    expect(audit.mock.calls[0]?.[0]).toMatchObject({
      action: "event.bridge.escalated",
      attempts: 3,
    });
    expect(rebuildConnection).toHaveBeenCalledTimes(1);
    expect(rebuildConnection).toHaveBeenCalledWith(
      "01J8ZQ5R0N7W9Y8X6V4T2S0K3",
    );
  });

  it("readback_gap_too_large 不自动重试/不升级（UI 既有提示路径）", async () => {
    const { send } = recorder();
    const readback = vi.fn(async () => {
      throw Object.assign(new Error("gap too large"), {
        code: "readback_gap_too_large",
      });
    });
    const rebuildConnection = vi.fn();
    const bridge = new MainEventBridge({
      send,
      readback,
      rebuildConnection,
      queueCapacity: 1,
    });
    bridge.push(makeEvent(1, "session.updated"));
    bridge.push(makeEvent(2, "session.updated"));
    await vi.waitFor(() => {
      expect(readback).toHaveBeenCalledTimes(1);
    });
    expect(bridge.snapshot().readbackGapTooLarge).toBe(1);
    expect(bridge.snapshot().connectionRebuilds).toBe(0);
    expect(rebuildConnection).not.toHaveBeenCalled();
  });

  it("dispose 后不再发送/补读（renderer reload/退出）", async () => {
    const { sent, send } = recorder();
    const readback = vi.fn(async () => ({ events: [], complete: true }));
    const bridge = new MainEventBridge({ send, readback, queueCapacity: 1 });
    bridge.dispose();
    bridge.push(makeEvent(1, "session.updated"));
    bridge.push(makeEvent(2, "session.updated"));
    bridge.flush();
    expect(sent).toHaveLength(0);
    await Promise.resolve();
    expect(readback).not.toHaveBeenCalled();
  });

  it("补读恢复按 seq 去重并保持顺序", async () => {
    const { sent, send } = recorder();
    const readback = vi.fn(async () => ({
      events: [makeEvent(2, "session.updated"), makeEvent(3, "session.updated")],
      complete: true,
    }));
    const bridge = new MainEventBridge({ send, readback, queueCapacity: 1 });
    bridge.push(makeEvent(1, "session.updated"));
    bridge.push(makeEvent(2, "session.updated"));
    await vi.waitFor(() => {
      expect(readback).toHaveBeenCalledTimes(1);
    });
    await vi.waitFor(() => {
      expect(bridge.snapshot().readbackEvents).toBe(2);
    });
    bridge.flush();
    expect(sent.map((entry) => entry.event.seq)).toEqual([1, 2, 3]);
  });
});
