/**
 * M7-04 DoD2：10 会话 × 50 delta/s 性能基准（不劣化于 P0 基线）。
 *
 * 口径（与 M3-01 DoD3 / M3-02 DoD4 / ADR-003 §2.1 同源）：
 * - 10 会话全部打开为标签并开启分屏（两个可见窗格；8 个后台标签不渲染消息）；
 * - 每轮 = 每会话 50 delta（1 秒流量）；共 {@link ROUNDS} 轮；
 * - **帧预算**：单轮 ingest + flush + React 提交耗时最小值 < 16ms（60fps；
 *   覆盖率插桩模式仅断言结构上界，口径同 M3-02）；
 * - **事件延迟**：单轮「入队 → 发布可见」耗时 P95 < 150ms（P0 基线 P95，ADR-003）；
 * - **批处理**：每会话 flush 次数 ≤ 轮数 + 2（16ms 合并窗口，无逐事件渲染）；
 * - **隔离**：各会话事件数逐条不丢、不重复；两个可见窗格只渲染自己的会话。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { LAYOUT_VERSION } from "./layout";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionIpc,
  SessionSummary,
} from "./session";
import { UI_LAYOUT_KEY, type SettingsIpc } from "./settings";

const SESSIONS = Array.from(
  { length: 10 },
  (_, index) => `01J8ZQ5R0N7W9Y8X6V4T2S0K${String(index).padStart(2, "0")}`,
);
const DELTAS_PER_SECOND = 50;
const ROUNDS = 10;
const FRAME_BUDGET_MS = 16;
/** P0 基线（ADR-003 §2.1：本机 Mock 事件回显 P95 < 150ms）。 */
const P0_LATENCY_P95_MS = 150;
const INSTRUMENTED = Boolean(process.env.AETHER_COVERAGE_LINES);

let eventCounter = 0;

function deltaEvent(sessionId: string, seq: number, text: string): AetherEvent {
  eventCounter += 1;
  return {
    v: 1,
    id: `01J${String(eventCounter).padStart(23, "0")}`,
    session_id: sessionId,
    run_id: `run-${sessionId}`,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type: "message.delta",
    payload: { message_id: `m-${sessionId}`, text },
  };
}

function startedEvent(sessionId: string): AetherEvent {
  eventCounter += 1;
  return {
    v: 1,
    id: `01J${String(eventCounter).padStart(23, "0")}`,
    session_id: sessionId,
    run_id: `run-${sessionId}`,
    runtime_id: "mock",
    seq: 0,
    ts: 1_700_000_000_000,
    type: "run.started",
    payload: { run_id: `run-${sessionId}` },
  };
}

function summary(index: number): SessionSummary {
  return {
    id: SESSIONS[index] as string,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: `性能会话 ${index}`,
    status: "idle",
    model: null,
    created_at: 1,
    updated_at: 100 - index,
    closed_at: null,
  };
}

function emptyPage(sessionId: string): MessagesPageResult {
  return {
    session_id: sessionId,
    max_seq: null,
    events: [],
    messages: [],
    complete: true,
  };
}

function fakeIpc(): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [
      {
        id: "mock",
        name: "Mock",
        kind: "mock",
        version: "0.1.0",
        protocol: "1.0",
        capabilities: [],
        enabled: true,
        status: "ready",
        status_reason: null,
        active_sessions: 0,
      } satisfies RuntimeInfo,
    ]),
    listSessions: vi.fn(async () =>
      Array.from({ length: SESSIONS.length }, (_, index) => summary(index)),
    ),
    createSession: vi.fn(),
    sendMessage: vi.fn(),
    interruptSession: vi.fn(),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(),
    messagesPage: vi.fn(async (input) => emptyPage(input.session_id)),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
  } as unknown as SessionIpc;
}

function fakeSettings(): SettingsIpc {
  return {
    get: vi.fn(async () => ({
      key: UI_LAYOUT_KEY,
      value: JSON.stringify({ version: LAYOUT_VERSION, tabs: [], active: null, split: null }),
    })),
    set: vi.fn(async (key: string, value: unknown) => ({ key, value })),
  } as unknown as SettingsIpc;
}

function percentile(samples: number[], p: number): number {
  const ordered = [...samples].sort((left, right) => left - right);
  const index = Math.min(ordered.length - 1, Math.ceil((p / 100) * ordered.length) - 1);
  return ordered[Math.max(0, index)] ?? 0;
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("工作台性能基准（M7-04 DoD2）", () => {
  it("10 会话 × 50 delta/s：标签+分屏下帧预算/事件延迟不劣化于 P0 基线", async () => {
    const flushCounts = new Map<string, number>();
    const store = new EventStore({
      onFlush: (sessionId) => {
        flushCounts.set(sessionId, (flushCounts.get(sessionId) ?? 0) + 1);
      },
    });

    render(
      <SessionWorkbench
        store={store}
        ipc={fakeIpc()}
        settingsIpc={fakeSettings()}
      />,
    );

    // 打开全部 10 个标签（切换的会话自动追加标签）。
    await waitFor(() => expect(screen.getAllByTestId("session-item").length).toBe(10), {
      timeout: 5_000,
    });
    for (const sessionId of SESSIONS) {
      const item = screen
        .getAllByTestId("session-item")
        .find((node) => node.getAttribute("data-session-id") === sessionId);
      fireEvent.click(item as HTMLElement);
    }
    await waitFor(() => expect(screen.getAllByTestId("session-tab").length).toBe(10), {
      timeout: 5_000,
    });

    // 分屏：最后点击的会话（主窗格）+ 第一个标签（第二窗格）。
    fireEvent.click(screen.getByTestId("split-toggle"));
    await waitFor(() => expect(screen.getByTestId("split-pane")).toBeTruthy(), {
      timeout: 5_000,
    });
    const primaryId = screen.getByTestId("pane-primary").getAttribute("data-session-id");
    const splitId = screen.getByTestId("split-pane").getAttribute("data-session-id");
    expect(primaryId).toBe(SESSIONS.at(-1));
    expect(splitId).toBe(SESSIONS[0]);

    // 基线事件（run.started）先发布（每会话 1 条，seq=0）。
    act(() => {
      for (const sessionId of SESSIONS) {
        store.ingest(startedEvent(sessionId));
      }
      store.flush();
    });

    const samples: number[] = [];
    for (let round = 0; round < ROUNDS; round += 1) {
      const startedAt = performance.now();
      act(() => {
        for (const sessionId of SESSIONS) {
          for (let index = 0; index < DELTAS_PER_SECOND; index += 1) {
            const seq = round * DELTAS_PER_SECOND + index + 1;
            store.ingest(deltaEvent(sessionId, seq, `s${sessionId.slice(-2)}-${seq},`));
          }
        }
        store.flush();
      });
      samples.push(performance.now() - startedAt);
    }

    const totalPerSession = ROUNDS * DELTAS_PER_SECOND;
    // 隔离 + 无丢失/无重复：逐会话事件数 = run.started + N delta，seq 连续。
    for (const sessionId of SESSIONS) {
      const state = store.getState(sessionId);
      expect(state.events).toHaveLength(totalPerSession + 1);
      expect(state.lastSeq).toBe(totalPerSession);
      expect(state.gap).toBe(false);
      const seqs = state.events.map((item) => item.seq);
      expect(new Set(seqs).size).toBe(totalPerSession + 1);
      expect(seqs).toEqual(Array.from({ length: totalPerSession + 1 }, (_, index) => index));
      // 批处理：flush 次数 ≤ 轮数 + 2（16ms 窗口合并，无逐事件渲染）。
      expect(flushCounts.get(sessionId) ?? 0).toBeLessThanOrEqual(ROUNDS + 2);
    }

    // 可见窗格只渲染自己的会话（后台标签不渲染消息）。
    const primaryBubble = screen.getByTestId("message-bubble");
    expect(primaryBubble.textContent).toContain(`s${primaryId?.slice(-2)}-${totalPerSession},`);
    expect(primaryBubble.textContent).not.toContain(`s${splitId?.slice(-2)}-`);
    const splitBubble = screen.getByTestId("split-message-bubble");
    expect(splitBubble.textContent).toContain(`s${splitId?.slice(-2)}-${totalPerSession},`);
    expect(splitBubble.textContent).not.toContain(`s${primaryId?.slice(-2)}-`);
    expect(screen.getAllByTestId("message-bubble")).toHaveLength(1);
    expect(screen.getAllByTestId("split-message-bubble")).toHaveLength(1);

    const best = Math.min(...samples);
    const p95 = percentile(samples, 95);
    console.log(
      `[m7-04 DoD2] 10 会话 × 50 delta/s（${ROUNDS} 轮）单轮 ingest+flush+提交耗时（ms）：` +
        `样本=${samples.map((value) => value.toFixed(2)).join(",")} 最小=${best.toFixed(2)} ` +
        `P95=${p95.toFixed(2)} 帧预算=${FRAME_BUDGET_MS} 延迟基线(P95)=${P0_LATENCY_P95_MS}`,
    );

    // 事件延迟：P95 不劣化于 P0 基线（ADR-003：P95 <150ms）。
    expect(p95).toBeLessThan(P0_LATENCY_P95_MS);
    if (INSTRUMENTED) {
      console.log("[m7-04 DoD2] 覆盖率插桩模式：仅断言结构上界，计时不参与判定");
    } else {
      // 帧预算：结构（DOM 只渲染两个窗格）与提交耗时同时满足 60fps 预算。
      expect(best).toBeLessThan(FRAME_BUDGET_MS);
    }

    store.dispose();
  });
});
