/**
 * M7-01 DoD5（UI）：运行时启停开关与禁用二次确认
 * （锚点 `runtime-toggle` / `runtime-disable-confirm`；ADR-015 决策 3/6、附录 E）。
 *
 * 覆盖：
 * - 开关语义（`data-enabled`/`data-status`/`aria-checked`）：偏好关 + disabled →
 *   点击启用（`runtime_enable`）；运行中 + 偏好开 → 点击禁用；
 * - 在途 `active_sessions > 0` → 二次确认（`data-count`），确认携
 *   `confirm_terminate=true` 重试、取消不落任何命令；
 * - 服务端竞态兜底：无列表计数但返回 `runtime_busy` → 解析错误文案计数弹确认；
 * - `user_disabled` 边界说明（附录 E-2）与故障禁用（偏好开）开关锁定；
 * - DoD0 前端消费：`runtime.status_changed` 事件到达 → 运行时注册表即时刷新。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import type { AetherEventListener } from "./eventBridge";
import { publishHealthState, resetHealthBusForTests } from "./healthBus";
import type { PermissionIpc } from "./permission";
import { RuntimePanel } from "./RuntimePanel";
import { SessionWorkbench } from "./SessionWorkbench";
import type { MessagesPageResult, RuntimeInfo, SessionIpc, SessionSummary } from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

function runtime(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
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
    active_sessions: 0,
    ...overrides,
  };
}

function fakePermissionIpc(overrides: Partial<PermissionIpc> = {}): PermissionIpc {
  return {
    pendingPermissions: vi.fn(async () => []),
    resolvePermission: vi.fn(),
    retryRuntime: vi.fn(),
    enableRuntime: vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    })),
    disableRuntime: vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "disabled",
      status: "disabled",
      status_reason: "user_disabled",
    })),
    ...overrides,
  };
}

function sessionSummary(): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "启停会话",
    status: "idle",
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION, max_seq: null, events: [], messages: [], complete: true };
}

function fakeSessionIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    listSessions: vi.fn(async () => [sessionSummary()]),
    createSession: vi.fn(async () => sessionSummary()),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION, interrupted_run: null })),
    disposeSession: vi.fn(async () => ({ session_id: SESSION, status: "completed" })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(),
    ...overrides,
  } as unknown as SessionIpc;
}

afterEach(() => {
  cleanup();
  resetHealthBusForTests();
});

describe("M7-01 S-04 启停开关（runtime-toggle）", () => {
  it("无在途：点击开关禁用（不带确认标志），成功后刷新注册表", async () => {
    const disableRuntime = vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "disabled",
      status: "disabled",
      status_reason: "user_disabled",
    }));
    const onRefresh = vi.fn();
    render(
      <RuntimePanel
        runtimes={[runtime({ active_sessions: 0 })]}
        ipc={fakePermissionIpc({ disableRuntime })}
        onRefresh={onRefresh}
      />,
    );
    const toggle = screen.getByTestId("runtime-toggle");
    expect(toggle.getAttribute("data-enabled")).toBe("true");
    expect(toggle.getAttribute("data-status")).toBe("ready");
    fireEvent.click(toggle);
    await waitFor(() => expect(disableRuntime).toHaveBeenCalledWith("mock", false));
    await waitFor(() => expect(onRefresh).toHaveBeenCalled());
    expect(screen.queryByTestId("runtime-disable-confirm")).toBeNull();
    expect(screen.getByTestId("runtime-control-notice").textContent).toContain("已禁用");
  });

  it("有在途（active_sessions=2）：先弹二次确认（data-count），确认后携 confirm_terminate", async () => {
    const disableRuntime = vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "disabled",
      status: "disabled",
      status_reason: "user_disabled",
    }));
    render(
      <RuntimePanel
        runtimes={[runtime({ active_sessions: 2 })]}
        ipc={fakePermissionIpc({ disableRuntime })}
        onRefresh={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByTestId("runtime-toggle"));
    const dialog = await screen.findByTestId("runtime-disable-confirm");
    expect(dialog.getAttribute("data-count")).toBe("2");
    expect(disableRuntime).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("runtime-disable-confirm-accept"));
    await waitFor(() => expect(disableRuntime).toHaveBeenCalledWith("mock", true));
    await waitFor(() => expect(screen.queryByTestId("runtime-disable-confirm")).toBeNull());
  });

  it("取消二次确认：不落任何命令、保留现状", async () => {
    const disableRuntime = vi.fn();
    render(
      <RuntimePanel
        runtimes={[runtime({ active_sessions: 1 })]}
        ipc={fakePermissionIpc({ disableRuntime })}
        onRefresh={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByTestId("runtime-toggle"));
    await screen.findByTestId("runtime-disable-confirm");
    fireEvent.click(screen.getByTestId("runtime-disable-confirm-cancel"));
    expect(screen.queryByTestId("runtime-disable-confirm")).toBeNull();
    expect(disableRuntime).not.toHaveBeenCalled();
  });

  it("服务端竞态兜底：runtime_busy → 解析错误文案计数弹确认并携确认重试", async () => {
    const disableRuntime = vi
      .fn()
      .mockRejectedValueOnce({
        code: "runtime_busy",
        field: "confirm_terminate",
        message: "运行时 mock 有 3 个在途会话；禁用将终止在途任务。确认继续请携 confirm_terminate=true 重试",
      })
      .mockResolvedValueOnce({
        runtime_id: "mock",
        outcome: "disabled",
        status: "disabled",
        status_reason: "user_disabled",
      });
    render(
      <RuntimePanel
        runtimes={[runtime({ active_sessions: undefined })]}
        ipc={fakePermissionIpc({ disableRuntime })}
        onRefresh={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByTestId("runtime-toggle"));
    await waitFor(() => expect(disableRuntime).toHaveBeenCalledWith("mock", false));
    const dialog = await screen.findByTestId("runtime-disable-confirm");
    expect(dialog.getAttribute("data-count")).toBe("3");
    fireEvent.click(screen.getByTestId("runtime-disable-confirm-accept"));
    await waitFor(() => expect(disableRuntime).toHaveBeenLastCalledWith("mock", true));
  });

  it("user_disabled：开关关 → 点击启用（runtime_enable）；展示边界说明", async () => {
    const enableRuntime = vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    }));
    render(
      <RuntimePanel
        runtimes={[
          runtime({
            enabled: false,
            status: "disabled",
            status_reason: "user_disabled",
            active_sessions: 0,
          }),
        ]}
        ipc={fakePermissionIpc({ enableRuntime })}
        onRefresh={vi.fn()}
      />,
    );
    const toggle = screen.getByTestId("runtime-toggle");
    expect(toggle.getAttribute("data-enabled")).toBe("false");
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    expect(screen.getByTestId("runtime-user-disabled-hint").textContent).toContain(
      "会话与数据保留",
    );
    fireEvent.click(toggle);
    await waitFor(() => expect(enableRuntime).toHaveBeenCalledWith("mock"));
  });

  it("故障禁用（偏好仍为开）：开关锁定，走既有 retry/enable 恢复入口", async () => {
    render(
      <RuntimePanel
        runtimes={[
          runtime({
            enabled: true,
            status: "disabled",
            status_reason: "start_failed",
          }),
        ]}
        ipc={fakePermissionIpc()}
        onRefresh={vi.fn()}
      />,
    );
    expect((screen.getByTestId("runtime-toggle") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("runtime-retry")).toBeTruthy();
  });
});

describe("M7-01 DoD0 前端消费（runtime.status_changed → 注册表刷新）", () => {
  it("事件到达（合成归段 runtime:<id>）→ runtimes_list 重取，徽标即时变化", async () => {
    let listRuntimesCalls = 0;
    let current: RuntimeInfo[] = [runtime({ status: "ready" })];
    const ipc = fakeSessionIpc({
      listRuntimes: vi.fn(async () => {
        listRuntimesCalls += 1;
        return current;
      }),
    });
    let handler: ((event: AetherEvent) => void) | null = null;
    const listener: AetherEventListener = async (next) => {
      handler = next;
      return () => {
        handler = null;
      };
    };
    render(
      <SessionWorkbench
        store={new EventStore()}
        ipc={ipc}
        permissionIpc={fakePermissionIpc()}
        eventListener={listener}
      />,
    );
    await screen.findByTestId("runtime-option");
    const badge = screen.getByTestId("runtime-badge");
    expect(badge.getAttribute("data-status")).toBe("ready");
    const before = listRuntimesCalls;

    // 运行时被禁用（事件携带合成会话归段）→ 触发刷新 → 徽标与选择器即时更新。
    current = [runtime({ enabled: false, status: "disabled", status_reason: "user_disabled" })];
    await act(async () => {
      handler?.({
        v: 1,
        id: "01J00000000000000000000EV1",
        session_id: "runtime:mock",
        run_id: null,
        runtime_id: "mock",
        seq: 1,
        ts: 1_700_000_000_001,
        type: "runtime.status_changed",
        payload: {
          runtime_id: "mock",
          from: "ready",
          to: "disabled",
          reason: "user_disabled",
        },
      } as AetherEvent);
    });
    await waitFor(() => expect(listRuntimesCalls).toBeGreaterThan(before));
    await waitFor(() =>
      expect(screen.getByTestId("runtime-badge").getAttribute("data-status")).toBe("disabled"),
    );
  });

  it("非运行时事件不触发注册表重取（过滤）", async () => {
    const listRuntimes = vi.fn(async () => [runtime()]);
    const ipc = fakeSessionIpc({ listRuntimes });
    let handler: ((event: AetherEvent) => void) | null = null;
    const listener: AetherEventListener = async (next) => {
      handler = next;
      return () => {
        handler = null;
      };
    };
    render(
      <SessionWorkbench
        store={new EventStore()}
        ipc={ipc}
        permissionIpc={fakePermissionIpc()}
        eventListener={listener}
      />,
    );
    await screen.findByTestId("runtime-option");
    const before = listRuntimes.mock.calls.length;
    await act(async () => {
      handler?.({
        v: 1,
        id: "01J00000000000000000000EV2",
        session_id: SESSION,
        run_id: null,
        runtime_id: "mock",
        seq: 1,
        ts: 1_700_000_000_002,
        type: "log",
        payload: { level: "info", message: "x" },
      } as AetherEvent);
    });
    await act(async () => {
      publishHealthState({
        status: "normal",
        report: { storage_state: "normal", write_queue_depth: 0, runtimes: null, ts: 3 },
        error: null,
        errorCode: null,
      });
    });
    expect(listRuntimes.mock.calls.length).toBe(before);
  });
});
