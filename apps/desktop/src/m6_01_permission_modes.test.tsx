/**
 * M6-01 会话级权限模式（UI 面；ADR-014 决策 3 / M6-01 DoD8）与 `always` 决议
 * （ADR-019；UI-UX §2.4/§7.3）。
 *
 * 覆盖（props 注入替身模式，同 m3_03）：
 * - 无会话：选择器禁用 + 空态；有会话：`data-mode` 回显；
 * - 点选档位 → `setPermissionMode(session, mode)`；新建会话透传 `permission_mode`；
 * - `permission-mode-boundary` 常驻文案关键片段（F-3）且不随档位切换消失；
 * - `permission-allow-always` → `resolvePermission(id, "always")`；已决议「已允许（始终）」。
 */
import type { AetherEvent } from "@aether/protocol";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import type {
  PermissionDecision,
  PermissionIpc,
  PendingPermission,
} from "./permission";
import { PermissionModeSelector } from "./PermissionModeSelector";
import { PermissionPanel } from "./PermissionPanel";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionCreateInput,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";

const SESSION_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";
const REQUEST_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1P";
const TICKET_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1T";
const WAIT = { timeout: 5000 };

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
    session_id: SESSION_A,
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

function sessionSummary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: SESSION_A,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "权限模式会话",
    status: "idle" as SessionStatus,
    model: null,
    thinking_depth: 2,
    permission_mode: "workspace_write",
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    ...overrides,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION_A, max_seq: null, events: [], messages: [], complete: true };
}

/** 会话替身：`setPermissionMode` 更新本地会话行（模拟后端回显，供 refresh 同步）。 */
function fakeSessionIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  const sessions: SessionSummary[] = [sessionSummary()];
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    searchSessions: vi.fn(async () => []),
    renameSession: vi.fn(async (sessionId: string, title: string) => ({
      session_id: sessionId,
      title,
      updated_at: 0,
    })),
    deleteSession: vi.fn(async (sessionId: string) => ({ session_id: sessionId, deleted: true })),
    listSessions: vi.fn(async () => sessions.map((session) => ({ ...session }))),
    createSession: vi.fn(async (input: SessionCreateInput) => {
      const created = sessionSummary({
        id: SESSION_B,
        title: input.title,
        permission_mode: input.permission_mode ?? "workspace_write",
      });
      sessions.push(created);
      return { ...created };
    }),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION_A,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION_A, interrupted_run: null })),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(async () => ({
      session_id: SESSION_A,
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(async () => ({
      session_id: SESSION_A,
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => {
      const target = sessions.find((session) => session.id === sessionId);
      const previous = target?.permission_mode ?? "workspace_write";
      if (target) {
        target.permission_mode = mode;
      }
      return {
        session_id: sessionId,
        permission_mode: mode,
        previous_permission_mode: previous,
      };
    }),
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
    retryRuntime: vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    })),
    enableRuntime: vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "ready",
      status: "ready",
    })),
    // M7-01/ADR-015：启停开关命令面（本文件不消费；缺省替身保持接口完整）。
    disableRuntime: vi.fn(async (runtimeId: string) => ({
      runtime_id: runtimeId,
      outcome: "disabled",
      status: "disabled",
      status_reason: "user_disabled",
    })),
    ...overrides,
  };
}

function pendingItem(overrides: Partial<PendingPermission> = {}): PendingPermission {
  return {
    id: TICKET_A,
    request_id: REQUEST_A,
    session_id: SESSION_A,
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
  vi.clearAllMocks();
});

describe("M6-01 权限模式选择器", () => {
  it("无会话：禁用 + 空态；有会话：data-mode 回显三档选项", () => {
    const onChange = vi.fn();
    const { unmount } = render(
      <PermissionModeSelector sessionId={null} value={null} onChange={onChange} />,
    );
    const container = screen.getByTestId("permission-mode-selector");
    expect(container.getAttribute("data-disabled")).toBe("true");
    expect(container.getAttribute("data-mode")).toBe("");
    expect((screen.getByTestId("permission-mode-toggle") as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(screen.getByTestId("permission-mode-empty").textContent).toContain("无会话");
    for (const item of screen.getAllByTestId("permission-mode-item")) {
      expect((item as HTMLButtonElement).disabled).toBe(true);
    }
    unmount();

    render(
      <PermissionModeSelector sessionId={SESSION_A} value="read_only" onChange={onChange} />,
    );
    const active = screen.getByTestId("permission-mode-selector");
    expect(active.getAttribute("data-disabled")).toBe("false");
    expect(active.getAttribute("data-mode")).toBe("read_only");
    expect(screen.queryByTestId("permission-mode-empty")).toBeNull();
    expect(
      screen
        .getAllByTestId("permission-mode-item")
        .map((item) => item.getAttribute("data-mode")),
    ).toEqual(["read_only", "workspace_write", "danger_full_access"]);
  });

  it("常驻边界声明（F-3 关键片段）；点选调用 onChange 后边界行仍在 DOM", () => {
    const onChange = vi.fn();
    render(
      <PermissionModeSelector sessionId={SESSION_A} value="workspace_write" onChange={onChange} />,
    );
    const boundaryText = () => screen.getByTestId("permission-mode-boundary").textContent ?? "";
    expect(boundaryText()).toContain("档位由核心权限门与本机运行时沙箱共同执行");
    expect(boundaryText()).toContain("权限门只约束经协议上报的工具调用");
    expect(boundaryText()).toContain(
      "适配器进程内行为（含其自身执行的命令）不受本门约束（信任级，非沙箱）",
    );
    expect(boundaryText()).toContain("『完全权限』= 运行时可读写任意路径、执行命令与联网");
    expect(boundaryText()).toContain("『仅查看／工作区内修改』档本门不开放命令与网络通道");

    fireEvent.click(screen.getByTestId("permission-mode-toggle"));
    const top = screen
      .getAllByTestId("permission-mode-item")
      .find((item) => item.getAttribute("data-mode") === "danger_full_access");
    fireEvent.click(top as HTMLElement);
    expect(onChange).toHaveBeenCalledWith("danger_full_access");
    // 弹层可关闭但内容常驻：边界行不消失/不清空（R14）。
    expect(boundaryText()).toContain("档位由核心权限门与本机运行时沙箱共同执行");
    expect(boundaryText()).toContain("『仅查看／工作区内修改』档本门不开放命令与网络通道");
  });

  it("SessionWorkbench：点选档位 → setPermissionMode(session, mode)；新建会话透传 permission_mode", async () => {
    const ipc = fakeSessionIpc();
    const store = newStore();
    render(<SessionWorkbench store={store} ipc={ipc} permissionIpc={fakePermissionIpc()} />);
    fireEvent.click(await screen.findByTestId("session-item"));
    await screen.findByTestId("composer-input");
    await screen.findByTestId("runtime-option");

    expect(screen.getByTestId("permission-mode-selector").getAttribute("data-mode")).toBe(
      "workspace_write",
    );
    fireEvent.click(screen.getByTestId("permission-mode-toggle"));
    const readOnly = screen
      .getAllByTestId("permission-mode-item")
      .find((item) => item.getAttribute("data-mode") === "read_only");
    fireEvent.click(readOnly as HTMLElement);
    await waitFor(
      () => expect(ipc.setPermissionMode).toHaveBeenCalledWith(SESSION_A, "read_only"),
      WAIT,
    );
    await waitFor(
      () =>
        expect(screen.getByTestId("permission-mode-selector").getAttribute("data-mode")).toBe(
          "read_only",
        ),
      WAIT,
    );

    // 新建会话：档位随 `session_create.permission_mode` 透传（当前档位）。
    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => expect(ipc.createSession).toHaveBeenCalled(), WAIT);
    const input = vi.mocked(ipc.createSession).mock.calls[0]?.[0];
    expect(input?.permission_mode).toBe("read_only");
  });

  it("切换会话回显档位；非法值回退缺省 workspace_write", async () => {
    const sessions: SessionSummary[] = [
      sessionSummary({
        id: SESSION_A,
        title: "顶档会话",
        permission_mode: "danger_full_access",
        updated_at: 2,
      }),
      sessionSummary({
        id: SESSION_B,
        title: "非法档会话",
        permission_mode: "bogus_mode",
        updated_at: 1,
      }),
    ];
    const ipc = fakeSessionIpc({ listSessions: vi.fn(async () => sessions) });
    render(
      <SessionWorkbench store={newStore()} ipc={ipc} permissionIpc={fakePermissionIpc()} />,
    );
    const items = await screen.findAllByTestId("session-item", {}, WAIT);
    const itemOf = (sessionId: string) =>
      items.find((item) => item.getAttribute("data-session-id") === sessionId);

    fireEvent.click(itemOf(SESSION_A) as HTMLElement);
    await waitFor(
      () =>
        expect(screen.getByTestId("permission-mode-selector").getAttribute("data-mode")).toBe(
          "danger_full_access",
        ),
      WAIT,
    );
    fireEvent.click(itemOf(SESSION_B) as HTMLElement);
    await waitFor(
      () =>
        expect(screen.getByTestId("permission-mode-selector").getAttribute("data-mode")).toBe(
          "workspace_write",
        ),
      WAIT,
    );
  });
});

describe("M6-01 always 决议（PermissionPanel）", () => {
  it("「记住（始终允许）」→ resolvePermission(id, \"always\")；已决议「已允许（始终）」", async () => {
    let pending: PendingPermission[] = [pendingItem()];
    const resolvePermission = vi.fn(
      async (requestId: string, decision: PermissionDecision) => {
        pending = [];
        return {
          request_id: requestId,
          decision: decision === "deny" ? ("deny" as const) : ("allow" as const),
          scope: decision === "deny" ? null : decision,
          ticket_id: TICKET_A,
        };
      },
    );
    const ipc = fakePermissionIpc({
      pendingPermissions: vi.fn(async () => pending),
      resolvePermission,
    });
    const { unmount } = render(
      <PermissionPanel ipc={ipc} sessionId={SESSION_A} events={[]} />,
    );
    await screen.findByTestId("permission-card", {}, WAIT);
    const allowAlways = screen.getByTestId("permission-allow-always");
    expect(allowAlways.textContent).toContain("记住（始终允许）");
    fireEvent.click(allowAlways);
    await waitFor(
      () => expect(resolvePermission).toHaveBeenCalledWith(REQUEST_A, "always"),
      WAIT,
    );
    await waitFor(
      () => expect(screen.queryByTestId("permission-allow-always")).toBeNull(),
      WAIT,
    );
    unmount();

    // `permission.resolved`（scope=always）→ 已决议列表「已允许（始终）」。
    const events: AetherEvent[] = [
      event(
        1,
        "permission.requested",
        {
          request_id: REQUEST_A,
          resource: "fs.write",
          action: "write",
          target: "D:\\ws\\a.txt",
        },
        null,
      ),
      event(
        2,
        "permission.resolved",
        { request_id: REQUEST_A, decision: "allow", scope: "always" },
        null,
      ),
    ];
    render(<PermissionPanel ipc={fakePermissionIpc()} sessionId={SESSION_A} events={events} />);
    await waitFor(() => {
      const resolved = screen
        .getAllByTestId("permission-card")
        .find((node) => node.getAttribute("data-status") === "resolved");
      expect(resolved?.textContent).toContain("已允许（始终）");
    }, WAIT);
  });
});
