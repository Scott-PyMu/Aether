/**
 * `aether://event` 监听接线测试（M3-01；D7 单通道）。
 */
import type { AetherEvent } from "@aether/protocol";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  registerAetherEventListener,
  useAetherEventBridge,
  type AetherEventHandler,
} from "./eventBridge";
import { EventStore } from "./eventStore";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

function makeEvent(seq: number): AetherEvent {
  return {
    v: 1,
    id: `01J${String(seq).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: null,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type: "message.delta",
    payload: { seq },
  };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("eventBridge（M3-01）", () => {
  it("监听回调把事件接入 EventStore；解除函数可用", async () => {
    const store = new EventStore();
    const captured: AetherEventHandler[] = [];
    const dispose = vi.fn();
    const unlisten = await registerAetherEventListener(store, async (handler) => {
      captured.push(handler);
      return dispose;
    });

    const handler = captured[0];
    expect(handler).toBeTruthy();
    handler?.(makeEvent(1));
    store.flush();
    expect(store.getState(SESSION).events).toHaveLength(1);

    unlisten();
    expect(dispose).toHaveBeenCalledTimes(1);
    store.dispose();
  });

  it("注册成功 → listening；注册失败 → unavailable（不崩溃）", async () => {
    function Probe({ fail }: { fail: boolean }) {
      const status = useAetherEventBridge(new EventStore(), async () => {
        if (fail) {
          throw new Error("非 Tauri 环境");
        }
        return () => {};
      });
      return <p data-testid="bridge-status">{status}</p>;
    }

    const { unmount } = render(<Probe fail={false} />);
    await vi.waitFor(() => {
      expect(screen.getByTestId("bridge-status").textContent).toBe("listening");
    });
    unmount();

    render(<Probe fail={true} />);
    await vi.waitFor(() => {
      expect(screen.getByTestId("bridge-status").textContent).toBe(
        "unavailable",
      );
    });
  });
});
