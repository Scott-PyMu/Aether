/**
 * M3-03 权限中心与运行状态面板（前端集成；真实 React + EventStore）：
 * - DoD1：待审批卡（原文 + 规范化对照）→ 允许/拒绝 → `permission_resolve`；
 *   超时倒计时与「已超时自动拒绝（300s）」呈现；决议失败结构化错误；
 * - DoD2：同会话并发 ask ≤1 激活 + 排队展示；
 * - DoD3：降级横幅/存储指示（标志注入）+ 发送禁用；disabled 运行时不可创建会话；
 * - S-04：运行时面板 retry/enable 与 untrusted/version_mismatch 修复说明；
 * - Q8：顶栏合并（唯一 status-bar + 运行时徽标/待审批计数/存储指示）。
 */
import type { AetherEvent } from "@aether/protocol";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { HealthMonitor } from "./HealthMonitor";
import { publishHealthState, resetHealthBusForTests } from "./healthBus";
import type { PermissionDecision, PermissionIpc, PendingPermission } from "./permission";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";
import type { HealthState } from "./useHealthPolling";

vi.mock("./useHealthPolling", () => ({ useHealthPolling: vi.fn() }));

const { useHealthPolling } = await import("./useHealthPolling");
const pollingMock = vi.mocked(useHealthPolling);

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const REQUEST_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1P";
const REQUEST_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K2P";
const REQUEST_C = "01J8ZQ5R0N7W9Y8X6V4T2S0K3P";
const TICKET_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1T";

let idCounter = 0;

function event(
  seq: number,
  type: string,
  payload: unknown,
  runId: string | null,
  ts?: number,
): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: runId,
    runtime_id: "mock",
    seq,
    ts: ts ?? 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

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
    ...overrides,
  };
}

function sessionSummary(status: SessionStatus = "idle"): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "权限会话",
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
    disposeSession: vi.fn(async () => ({ session_id: SESSION, status: "completed" as SessionStatus })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(async () => ({
      session_id: SESSION,
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
    ...overrides,
  };
}

function fakePermissionIpc(overrides: Partial<PermissionIpc> = {}): PermissionIpc {
  return {
    pendingPermissions: vi.fn(async () => []),
    resolvePermission: vi.fn(
      async (requestId: string, decision: PermissionDecision) => ({
        request_id: requestId,
        decision: decision === "deny" ? ("deny" as const) : ("allow" as const),
        scope: decision === "deny" ? null : decision,
        ticket_id: TICKET_A,
      }),
    ),
    retryRuntime: vi.fn(async (runtimeId) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    })),
    enableRuntime: vi.fn(async (runtimeId) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    })),
    ...overrides,
  };
}

function pendingItem(overrides: Partial<PendingPermission> = {}): PendingPermission {
  return {
    id: TICKET_A,
    request_id: REQUEST_A,
    session_id: SESSION,
    resource: "fs.write",
    action: "write",
    target: "D:\\ws\\a.txt",
    canonical_target: "D:\\ws\\a.txt",
    requested_at: Date.now(),
    timeout_ms: 300_000,
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
  vi.clearAllMocks();
});

async function renderWorkbench(
  options: { ipc?: SessionIpc; permissionIpc?: PermissionIpc; store?: EventStore } = {},
) {
  const store = options.store ?? newStore();
  render(
    <SessionWorkbench
      store={store}
      ipc={options.ipc ?? fakeSessionIpc()}
      permissionIpc={options.permissionIpc ?? fakePermissionIpc()}
    />,
  );
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
  return store;
}

function healthState(overrides: Partial<HealthState>): HealthState {
  return { status: "loading", report: null, error: null, errorCode: null, ...overrides };
}

describe("M3-03 权限中心", () => {
  it("DoD1：待审批卡展示原文与规范化对照 → 允许（once）→ IPC 决议 + 事件收口", async () => {
    let pending: PendingPermission[] = [pendingItem()];
    const permissionIpc = fakePermissionIpc({
      pendingPermissions: vi.fn(async () => pending),
      resolvePermission: vi.fn(async (requestId, decision) => {
        pending = [];
        return {
          request_id: requestId,
          decision: decision === "deny" ? ("deny" as const) : ("allow" as const),
          scope: decision === "deny" ? null : decision,
          ticket_id: TICKET_A,
        };
      }),
    });
    const store = await renderWorkbench({ permissionIpc });

    const card = await screen.findByTestId("permission-card");
    expect(card.getAttribute("data-status")).toBe("pending");
    expect(card.getAttribute("data-request-id")).toBe(REQUEST_A);
    expect(screen.getByTestId("permission-target-raw").textContent).toContain("D:\\ws\\a.txt");
    const canonical = screen.getByTestId("permission-target-canonical");
    expect(canonical.getAttribute("data-equal")).toBe("true");
    expect(canonical.textContent).toContain("规范化后一致");
    const note = screen.getByTestId("permission-timeout-note");
    expect(Number(note.getAttribute("data-remaining-ms"))).toBeGreaterThan(0);
    expect(note.getAttribute("data-expired")).toBe("false");
    // C6 边界声明（不得表述为已隔离适配器内部行为）。
    expect(card.textContent).toContain("权限门仅约束经线协议上报的工具调用");

    fireEvent.click(screen.getByTestId("permission-allow-once"));
    await waitFor(() =>
      expect(permissionIpc.resolvePermission).toHaveBeenCalledWith(REQUEST_A, "once"),
    );
    await waitFor(() => expect(screen.queryByTestId("permission-card")).toBeNull());

    // `permission.resolved` 事件收口为弱化结果态（保留展示）。
    act(() => {
      store.ingest(
        event(
          1,
          "permission.resolved",
          { request_id: REQUEST_A, decision: "allow", scope: "once" },
          null,
          1_700_000_001_000,
        ),
      );
      store.flush();
    });
    const resolved = await screen.findByTestId("permission-card");
    expect(resolved.getAttribute("data-status")).toBe("resolved");
    expect(resolved.textContent).toContain("已允许（本次）");
  });

  it("DoD1：拒绝走 deny；超时倒计时到 0 后以 timeout 收口", async () => {
    // 原文与规范化不一致（软链接解析等真实差异）→ data-equal=false。
    const permissionIpc = fakePermissionIpc({
      pendingPermissions: vi.fn(async () => [
        pendingItem({
          request_id: REQUEST_A,
          requested_at: 1_700_000_000_000,
          target: "D:\\ws\\link.txt",
          canonical_target: "D:\\ws\\real.txt",
        }),
        pendingItem({
          request_id: REQUEST_B,
          target: "D:\\ws\\timeout.txt",
          canonical_target: "D:\\ws\\timeout.txt",
          requested_at: 1_700_000_000_100,
        }),
      ]),
      resolvePermission: vi.fn(async (requestId, decision) => ({
        request_id: requestId,
        decision: decision === "deny" ? ("deny" as const) : ("allow" as const),
        scope: decision === "deny" ? null : decision,
        ticket_id: TICKET_A,
      })),
    });
    const store = await renderWorkbench({ permissionIpc });

    await screen.findByTestId("permission-card");
    const canonical = screen.getByTestId("permission-target-canonical");
    expect(canonical.getAttribute("data-equal")).toBe("false");
    expect(canonical.textContent).toContain("与原文不一致");

    fireEvent.click(screen.getByTestId("permission-deny"));
    await waitFor(() =>
      expect(permissionIpc.resolvePermission).toHaveBeenCalledWith(REQUEST_A, "deny"),
    );

    // 超时：请求在 300s 前发生 → 倒计时 0；决议事件恰好晚于 300s → data-status=timeout。
    act(() => {
      store.ingest(
        event(
          1,
          "permission.requested",
          {
            request_id: REQUEST_B,
            resource: "fs.write",
            action: "write",
            target: "D:\\ws\\timeout.txt",
          },
          null,
          1_700_000_000_100,
        ),
      );
      store.ingest(
        event(
          2,
          "permission.resolved",
          { request_id: REQUEST_B, decision: "deny", scope: null },
          null,
          1_700_000_300_100,
        ),
      );
      store.flush();
    });
    await waitFor(() => {
      const cards = screen.getAllByTestId("permission-card");
      const timeoutCard = cards.find((node) => node.getAttribute("data-status") === "timeout");
      expect(timeoutCard?.textContent).toContain("已超时自动拒绝（300s）");
    });
  });

  it("DoD1：决议失败（permission_not_pending）展示结构化错误并刷新清单", async () => {
    const pendingPermissions = vi.fn(async () => [] as PendingPermission[]);
    const permissionIpc = fakePermissionIpc({
      pendingPermissions,
      resolvePermission: vi.fn(async () => {
        throw {
          code: "invalid_value",
          message: "审批请求不在待决议队列（permission_not_pending）",
        };
      }),
    });
    await renderWorkbench({ permissionIpc });
    await screen.findByTestId("permission-empty");

    // 事件注入 pending，再触发一次清单校准（真实路径由 requested 事件驱动）。
    const store = stores[stores.length - 1];
    pendingPermissions.mockResolvedValueOnce([pendingItem()]);
    act(() => {
      store?.ingest(
        event(1, "permission.requested", {
          request_id: REQUEST_A,
          resource: "fs.write",
          action: "write",
          target: "D:\\ws\\a.txt",
        }, null),
      );
      store?.flush();
    });
    await screen.findByTestId("permission-card");
    fireEvent.click(screen.getByTestId("permission-allow-session"));
    const error = await screen.findByTestId("permission-error");
    expect(error.textContent).toContain("permission_not_pending");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
  });

  it("DoD2：同会话 3 条 ask → ≤1 激活 + 排队展示；决议后下一条激活", async () => {
    let pending: PendingPermission[] = [
      pendingItem({ request_id: REQUEST_A, requested_at: 1_700_000_000_000 }),
      pendingItem({ request_id: REQUEST_B, requested_at: 1_700_000_001_000 }),
      pendingItem({ request_id: REQUEST_C, requested_at: 1_700_000_002_000 }),
    ];
    const permissionIpc = fakePermissionIpc({
      pendingPermissions: vi.fn(async () => pending),
      resolvePermission: vi.fn(async (requestId, decision) => {
        pending = pending.filter((item) => item.request_id !== requestId);
        return {
          request_id: requestId,
          decision: decision === "deny" ? ("deny" as const) : ("allow" as const),
          scope: decision === "deny" ? null : decision,
          ticket_id: TICKET_A,
        };
      }),
    });
    await renderWorkbench({ permissionIpc });

    const queueCount = await screen.findByTestId("permission-queue-count");
    expect(queueCount.getAttribute("data-count")).toBe("3");
    const cards = screen.getAllByTestId("permission-card");
    const pendingCards = cards.filter((node) => node.getAttribute("data-status") === "pending");
    expect(pendingCards).toHaveLength(1);
    expect(pendingCards[0]?.getAttribute("data-request-id")).toBe(REQUEST_A);
    expect(screen.getByTestId("permission-queue-note").textContent).toContain("还有 2 条排队");

    fireEvent.click(screen.getByTestId("permission-deny"));
    await waitFor(() =>
      expect(permissionIpc.resolvePermission).toHaveBeenCalledWith(REQUEST_A, "deny"),
    );
    await waitFor(() => {
      const active = screen
        .getAllByTestId("permission-card")
        .find((node) => node.getAttribute("data-status") === "pending");
      expect(active?.getAttribute("data-request-id")).toBe(REQUEST_B);
    });
    expect(screen.getByTestId("permission-queue-note").textContent).toContain("还有 1 条排队");
  });
});

describe("M3-03 状态面板", () => {
  it("DoD3：降级横幅与存储指示（标志注入）+ 发送入口禁用", async () => {
    pollingMock.mockReturnValue(
      healthState({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          degrade_trigger: "write_failure",
        },
      }),
    );
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
        errorCode: null,
      });
    });
    const store = newStore();
    render(
      <>
        <HealthMonitor />
        <SessionWorkbench
          store={store}
          ipc={fakeSessionIpc()}
          permissionIpc={fakePermissionIpc()}
        />
      </>,
    );
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");

    expect(screen.getByTestId("storage-degraded").textContent).toContain("存储降级（只读）");
    expect(screen.getByTestId("storage-indicator").getAttribute("data-status")).toBe(
      "persist_degraded",
    );
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("composer-degraded-hint").textContent).toContain("存储降级");
  });

  it("DoD3：disabled 运行时不可选、不可创建会话（含选中后转 disabled 兜底）", async () => {
    let runtimeList: RuntimeInfo[] = [runtime({ status: "ready" })];
    const createSession = vi.fn(async () => sessionSummary());
    const ipc = fakeSessionIpc({
      listRuntimes: vi.fn(async () => runtimeList),
      createSession,
    });
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} permissionIpc={fakePermissionIpc()} />);

    const option = await screen.findByTestId("runtime-option");
    expect(option.getAttribute("data-selected")).toBe("true");

    // 运行时转为 disabled（health 摘要签名变化触发注册表刷新）→ 选中项保留但不可创建。
    runtimeList = [runtime({ status: "disabled", status_reason: "start_failed" })];
    act(() => {
      publishHealthState({
        status: "normal",
        report: {
          storage_state: "normal",
          write_queue_depth: 0,
          runtimes: [{ id: "mock", status: "disabled", status_reason: "start_failed" }],
          ts: 2,
        },
        error: null,
        errorCode: null,
      });
    });
    await waitFor(() =>
      expect(screen.getByTestId("runtime-option").getAttribute("data-status")).toBe("disabled"),
    );
    expect((screen.getByTestId("runtime-option") as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByTestId("session-create-submit"));
    const error = await screen.findByTestId("workbench-error");
    expect(error.textContent).toContain("已禁用");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(createSession).not.toHaveBeenCalled();
  });

  it("DoD3：无可用（全 disabled）运行时时选中为空且创建被拒", async () => {
    const ipc = fakeSessionIpc({
      listRuntimes: vi.fn(async () => [
        runtime({ id: "mock", status: "disabled", status_reason: "untrusted" }),
      ]),
    });
    render(
      <SessionWorkbench store={newStore()} ipc={ipc} permissionIpc={fakePermissionIpc()} />,
    );
    const option = await screen.findByTestId("runtime-option");
    expect(option.getAttribute("data-selected")).toBe("false");
    expect((option as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByTestId("session-create-submit"));
    const error = await screen.findByTestId("workbench-error");
    expect(error.textContent).toContain("请选择运行时");
  });

  it("S-04：运行时面板 retry/enable 入口与不可直接启用的修复说明", async () => {
    const retryRuntime = vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    }));
    const enableRuntime = vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    }));
    const ipc = fakeSessionIpc({
      listRuntimes: vi.fn(async () => [
        runtime({ id: "claude", name: "Claude Code", status: "disabled", status_reason: "start_failed" }),
        runtime({ id: "codex", name: "Codex", status: "disabled", status_reason: "crash_loop" }),
        runtime({ id: "dsh", name: "DSH", status: "disabled", status_reason: "untrusted" }),
        runtime({
          id: "vm",
          name: "版本不匹配运行时",
          status: "disabled",
          status_reason: "version_mismatch",
        }),
        runtime({ id: "ready", name: "就绪运行时" }),
      ]),
    });
    const permissionIpc = fakePermissionIpc({ retryRuntime, enableRuntime });
    render(<SessionWorkbench store={newStore()} ipc={ipc} permissionIpc={permissionIpc} />);
    await screen.findAllByTestId("runtime-panel-item");

    // start_failed → 重试；crash_loop → 重新启用；untrusted/version_mismatch → 无操作 + 说明。
    expect(screen.getAllByTestId("runtime-retry")).toHaveLength(1);
    expect(screen.getAllByTestId("runtime-enable")).toHaveLength(1);
    const remedies = screen.getAllByTestId("runtime-remedy").map((node) => node.textContent);
    expect(remedies).toHaveLength(2);
    expect(remedies.join(" ")).toContain("官方适配器");
    expect(remedies.join(" ")).toContain("版本不匹配");

    fireEvent.click(screen.getByTestId("runtime-retry"));
    await waitFor(() => expect(retryRuntime).toHaveBeenCalledWith("claude"));
    fireEvent.click(screen.getByTestId("runtime-enable"));
    await waitFor(() => expect(enableRuntime).toHaveBeenCalledWith("codex"));
    expect(await screen.findByTestId("runtime-control-notice")).toBeTruthy();
  });

  it("Q8 顶栏：唯一状态条 + 运行时徽标/待审批计数/存储指示", async () => {
    const sessionStatus = sessionSummary("running");
    const ipc = fakeSessionIpc({
      listRuntimes: vi.fn(async () => [
        runtime({ id: "claude", name: "Claude Code", status: "ready" }),
        runtime({ id: "codex", name: "Codex", status: "degraded", status_reason: "storage_backpressure" }),
      ]),
      listSessions: vi.fn(async () => [sessionStatus]),
    });
    const permissionIpc = fakePermissionIpc({
      pendingPermissions: vi.fn(async () => [pendingItem()]),
    });
    act(() => {
      publishHealthState({
        status: "normal",
        report: {
          storage_state: "normal",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
        },
        error: null,
        errorCode: null,
      });
    });
    render(<SessionWorkbench store={newStore()} ipc={ipc} permissionIpc={permissionIpc} />);
    fireEvent.click(await screen.findByTestId("session-item"));

    expect(screen.getByTestId("topbar")).toBeTruthy();
    expect(screen.getAllByTestId("status-bar")).toHaveLength(1);
    expect(screen.getByTestId("session-status").getAttribute("data-status")).toBe("running");
    expect(screen.getByTestId("run-status")).toBeTruthy();
    const badges = screen.getAllByTestId("runtime-badge");
    expect(badges.map((node) => node.getAttribute("data-status"))).toEqual(["ready", "degraded"]);
    await waitFor(() =>
      expect(screen.getByTestId("pending-permission-badge").getAttribute("data-count")).toBe("1"),
    );
    expect(screen.getByTestId("storage-indicator").getAttribute("data-status")).toBe("normal");

    // <1280px 抽屉开关（断点布局由 CSS 控制；此处断言开关状态契约）。
    const toggle = screen.getByTestId("right-panel-toggle");
    expect(screen.getByTestId("right-panel").getAttribute("data-open")).toBe("true");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(screen.getByTestId("right-panel").getAttribute("data-open")).toBe("false");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });
});
