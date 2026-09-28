/**
 * M3-06 崩溃恢复体验（前端）集成测试：
 * - DoD2：失败/中断 run 的气泡提供「重试」（`run_retry`，仅终态；降级期禁用）；
 * - DoD3：`persist_degraded` → 发送入口禁用 + 在途 run「已中断（存储降级）」提示；
 *   降级横幅经共享健康总线联动（注入 + E2E 口径）。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { publishHealthState, resetHealthBusForTests } from "./healthBus";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_FAILED = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";
const RUN_CANCELLED = "01J8ZQ5R0N7W9Y8X6V4T2S0K1S";

let idCounter = 0;

function event(
  seq: number,
  type: string,
  payload: unknown,
  runId: string | null,
): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: runId,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

function runtime(): RuntimeInfo {
  return {
    id: "mock",
    name: "Mock",
    kind: "mock",
    version: "0.1.0",
    protocol: "1.0",
    capabilities: [],
    enabled: true,
    status: "ready",
    status_reason: null,
  };
}

function sessionSummary(status: SessionStatus): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "恢复会话",
    status,
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION, max_seq: null, events: [], messages: [], complete: true };
}

function fakeIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    listSessions: vi.fn(async () => [sessionSummary("idle")]),
    createSession: vi.fn(async () => sessionSummary("idle")),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_FAILED,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION, interrupted_run: null })),
    disposeSession: vi.fn(async () => ({ session_id: SESSION, status: "completed" as SessionStatus })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(async () => ({
      session_id: SESSION,
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K2R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
    ...overrides,
  };
}

const stores: EventStore[] = [];

function newStore(): EventStore {
  const store = new EventStore();
  stores.push(store);
  return store;
}

afterEach(() => {
  cleanup();
  for (const store of stores.splice(0)) {
    store.dispose();
  }
  resetHealthBusForTests();
  vi.restoreAllMocks();
});

async function renderWithFailedRun(ipc: SessionIpc) {
  const store = newStore();
  render(<SessionWorkbench store={store} ipc={ipc} />);
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
  act(() => {
    store.ingest(event(1, "run.started", { run_id: RUN_FAILED }, RUN_FAILED));
    store.ingest(
      event(
        2,
        "run.failed",
        {
          run_id: RUN_FAILED,
          error: { code: "run_interrupted", message: "核心重启中断", recoverable: true },
        },
        RUN_FAILED,
      ),
    );
    store.flush();
  });
  return store;
}

describe("M3-06 崩溃恢复体验（前端）", () => {
  it("DoD2：失败 run 的气泡展示错误码与重试按钮；点击调用 run_retry", async () => {
    const retryRun = vi.fn(async () => ({
      session_id: SESSION,
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K2R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    }));
    const ipc = fakeIpc({ retryRun });
    await renderWithFailedRun(ipc);

    const outcome = await screen.findByTestId("run-outcome");
    expect(outcome.getAttribute("data-status")).toBe("failed");
    expect(outcome.textContent).toContain("运行失败：run_interrupted");
    const retry = screen.getByTestId("run-retry");
    expect(retry.getAttribute("data-run-id")).toBe(RUN_FAILED);
    expect((retry as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(retry);
    await waitFor(() => expect(retryRun).toHaveBeenCalledTimes(1));
    expect(retryRun).toHaveBeenCalledWith(RUN_FAILED);
  });

  it("DoD2：存储降级中断的 run 显示「已中断（存储降级）」+ 重试（降级期禁用）", async () => {
    const ipc = fakeIpc();
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} />);
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");
    act(() => {
      store.ingest(event(1, "run.started", { run_id: RUN_CANCELLED }, RUN_CANCELLED));
      store.ingest(
        event(
          2,
          "run.cancelled",
          { run_id: RUN_CANCELLED, reason: "persist_degraded" },
          RUN_CANCELLED,
        ),
      );
      store.flush();
    });
    const outcome = await screen.findByTestId("run-outcome");
    expect(outcome.getAttribute("data-status")).toBe("cancelled");
    expect(outcome.textContent).toContain("已中断（存储降级）");

    // 注入降级：发送入口禁用 + 提示；重试按钮禁用（修复后可用）。
    act(() => {
      publishHealthState({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          degrade_trigger: "write_failure",
        },
        error: null,
      });
    });
    await waitFor(() => {
      expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
    });
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(true);
    expect(screen.getByTestId("composer-degraded-hint").textContent).toContain("存储降级");
    expect((screen.getByTestId("run-retry") as HTMLButtonElement).disabled).toBe(true);
  });

  it("DoD2：重试失败展示结构化错误（不崩溃）", async () => {
    const retryRun = vi.fn(async () => {
      throw { code: "invalid_value", message: "run 非终态（run_not_retryable）" };
    });
    const ipc = fakeIpc({ retryRun });
    await renderWithFailedRun(ipc);

    fireEvent.click(await screen.findByTestId("run-retry"));
    const error = await screen.findByTestId("workbench-error");
    expect(error.textContent).toContain("run_not_retryable");
  });

  it("DoD3：正常态发送入口可用（降级联动不误伤）", async () => {
    const ipc = fakeIpc();
    render(<SessionWorkbench store={newStore()} ipc={ipc} />);
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");
    act(() => {
      publishHealthState({
        status: "normal",
        report: {
          storage_state: "normal",
          write_queue_depth: 0,
          runtimes: [],
          ts: 1,
        },
        error: null,
      });
    });
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "hi" } });
    await waitFor(() => {
      expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(false);
    });
    expect(screen.queryByTestId("composer-degraded-hint")).toBeNull();
  });
});
