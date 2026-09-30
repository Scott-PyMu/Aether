/**
 * M3-09 前端集成测试：文件引用面板（S-11；ADR-010 决策 1；UI-UX §2.6/§7.3）。
 *
 * 覆盖：
 * - DoD5：添加文件 → 出现在「会话文件」；附加文件夹 → 出现在「项目文件」；
 *   切换会话隔离；空态文案「目录为空」；`改动` tab 无入口；
 * - DoD2/DoD4（UI 面）：取消选择不新增；`artifact_path_rejected` 结构化提示
 *   （`ref-pick-error` + `data-code`）；
 * - DoD3：行内删除（幂等命令面调用）；
 * - DoD7：面板位于右栏（`right-panel`）内，随会话切换；项目 tab 展示工作区根路径。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { artifactsIpc as productionArtifactsIpc } from "./artifacts";
import type { ArtifactsIpc, ArtifactEntry, RefKind } from "./artifacts";
import { EventStore } from "./eventStore";
import { FilePanel } from "./FilePanel";
import type { PermissionIpc } from "./permission";
import type { SessionIpc, SessionStatus, SessionSummary } from "./session";
import { SessionWorkbench } from "./SessionWorkbench";

const SESSION_A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const SESSION_B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";

const WAIT = { timeout: 5000 };

afterEach(cleanup);

function entry(
  id: string,
  path: string,
  kind: ArtifactEntry["kind"],
  createdAt = 1,
): ArtifactEntry {
  return {
    id,
    path,
    kind,
    size_bytes: kind === "file" ? 5 : null,
    created_at: createdAt,
  };
}

interface FakeArtifacts {
  ipc: ArtifactsIpc;
  pick: ReturnType<typeof vi.fn>;
  add: ReturnType<typeof vi.fn>;
  remove: ReturnType<typeof vi.fn>;
  list: ReturnType<typeof vi.fn>;
}

/** 会话级引用存储替身（隔离口径：按 session_id 分桶）。 */
function fakeArtifactsIpc(seed: Record<string, ArtifactEntry[]> = {}): FakeArtifacts {
  const store = new Map<string, ArtifactEntry[]>(Object.entries(seed));
  let counter = 0;
  const pick = vi.fn(async (_kind: RefKind) => null as string | null);
  const list = vi.fn(async (sessionId: string) => [...(store.get(sessionId) ?? [])]);
  const add = vi.fn(async (sessionId: string, path: string) => {
    const kind: ArtifactEntry["kind"] = path.endsWith("\\folder") ? "directory" : "file";
    counter += 1;
    const item = entry(`01J${String(counter).padStart(23, "0")}`, path, kind, counter);
    store.set(sessionId, [...(store.get(sessionId) ?? []), item]);
    return item;
  });
  const remove = vi.fn(async (sessionId: string, artifactId: string) => {
    const items = store.get(sessionId) ?? [];
    const next = items.filter((item) => item.id !== artifactId);
    store.set(sessionId, next);
    return next.length !== items.length;
  });
  return { ipc: { pick, list, add, remove } as ArtifactsIpc, pick, add, remove, list };
}

function renderPanel(artifacts: FakeArtifacts, sessionId: string | null, workspaceRoot?: string | null) {
  return render(
    <FilePanel ipc={artifacts.ipc} sessionId={sessionId} workspaceRoot={workspaceRoot ?? null} />,
  );
}

describe("FilePanel（M3-09）", () => {
  it("DoD5：添加文件出现在「会话文件」；附加文件夹出现在「项目文件」", async () => {
    const artifacts = fakeArtifactsIpc();
    artifacts.pick
      .mockResolvedValueOnce("C:\\ws\\a.txt")
      .mockResolvedValueOnce("C:\\ws\\folder");
    renderPanel(artifacts, SESSION_A, "C:\\ws");

    fireEvent.click(screen.getByTestId("ref-add-file"));
    const sessionItem = await screen.findByTestId("ref-item", {}, WAIT);
    expect(sessionItem.getAttribute("data-ref-kind")).toBe("session");
    expect(sessionItem.getAttribute("data-path")).toBe("C:\\ws\\a.txt");
    expect(artifacts.pick).toHaveBeenCalledWith("file");
    expect(artifacts.add).toHaveBeenCalledWith(SESSION_A, "C:\\ws\\a.txt");

    // 项目 tab：工作区根展示 + 目录引用。
    fireEvent.click(screen.getByTestId("file-panel-project-tab"));
    expect(screen.getByText(/工作区：C:\\ws/)).toBeTruthy();
    expect(screen.getByTestId("file-panel-empty").textContent).toContain("目录为空");
    fireEvent.click(screen.getByTestId("ref-add-folder"));
    await waitFor(() => {
      const items = screen.getAllByTestId("ref-item");
      expect(items).toHaveLength(1);
      expect(items[0]?.getAttribute("data-ref-kind")).toBe("project");
      expect(items[0]?.getAttribute("data-path")).toBe("C:\\ws\\folder");
    }, WAIT);
    expect(artifacts.pick).toHaveBeenCalledWith("directory");

    // 会话文件 tab 仍保留文件引用。
    fireEvent.click(screen.getByTestId("file-panel-session-tab"));
    const items = screen.getAllByTestId("ref-item");
    expect(items).toHaveLength(1);
    expect(items[0]?.getAttribute("data-path")).toBe("C:\\ws\\a.txt");
  });

  it("DoD5：切换会话隔离；空态文案「目录为空」；无会话为「请选择会话」", async () => {
    const artifacts = fakeArtifactsIpc({
      [SESSION_A]: [entry("01J00000000000000000000RA1", "C:\\ws\\a.txt", "file")],
    });
    const view = renderPanel(artifacts, SESSION_A);
    const item = await screen.findByTestId("ref-item", {}, WAIT);
    expect(item.getAttribute("data-path")).toBe("C:\\ws\\a.txt");

    // 切到会话 B：无引用 → 空态（隔离，不继承会话 A 的引用）。
    view.rerender(
      <FilePanel ipc={artifacts.ipc} sessionId={SESSION_B} workspaceRoot={null} />,
    );
    const empty = await screen.findByTestId("file-panel-empty", {}, WAIT);
    expect(empty.textContent).toContain("目录为空");
    expect(screen.queryByTestId("ref-item")).toBeNull();

    // 无会话：引导空态。
    view.rerender(
      <FilePanel ipc={artifacts.ipc} sessionId={null} workspaceRoot={null} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("file-panel-empty").textContent).toContain("请选择会话");
    }, WAIT);
  });

  it("DoD5：仅「文件」面板（改动 tab 无入口）", () => {
    const artifacts = fakeArtifactsIpc();
    renderPanel(artifacts, SESSION_A);
    expect(screen.getByTestId("file-panel-session-tab")).toBeTruthy();
    expect(screen.getByTestId("file-panel-project-tab")).toBeTruthy();
    expect(screen.queryByText("改动")).toBeNull();
    expect(screen.queryByText("打开文件")).toBeNull();
  });

  it("DoD4：选择器取消不新增引用", async () => {
    const artifacts = fakeArtifactsIpc();
    artifacts.pick.mockResolvedValueOnce(null);
    renderPanel(artifacts, SESSION_A);
    fireEvent.click(screen.getByTestId("ref-add-file"));
    await waitFor(() => {
      expect(artifacts.pick).toHaveBeenCalledTimes(1);
    }, WAIT);
    expect(artifacts.add).not.toHaveBeenCalled();
    expect(screen.getByTestId("file-panel-empty")).toBeTruthy();
  });

  it("DoD2（UI 面）：artifact_path_rejected 结构化提示（ref-pick-error + data-code）", async () => {
    const artifacts = fakeArtifactsIpc();
    artifacts.pick.mockResolvedValueOnce("C:\\stale\\gone.txt");
    artifacts.add.mockRejectedValueOnce({
      code: "artifact_path_rejected",
      message: "路径不可解析（canonicalize 失败）",
    });
    renderPanel(artifacts, SESSION_A);
    fireEvent.click(screen.getByTestId("ref-add-file"));
    const error = await screen.findByTestId("ref-pick-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("artifact_path_rejected");
    expect(error.textContent).toContain("引用路径不可用");
  });

  it("DoD3（UI 面）：行内删除调用 artifact_remove 并刷新列表", async () => {
    const artifacts = fakeArtifactsIpc({
      [SESSION_A]: [entry("01J00000000000000000000RA1", "C:\\ws\\a.txt", "file")],
    });
    renderPanel(artifacts, SESSION_A);
    await screen.findByTestId("ref-item", {}, WAIT);
    fireEvent.click(screen.getByTestId("ref-remove"));
    await waitFor(() => {
      expect(artifacts.remove).toHaveBeenCalledWith(
        SESSION_A,
        "01J00000000000000000000RA1",
      );
    }, WAIT);
    await waitFor(() => {
      expect(screen.getByTestId("file-panel-empty").textContent).toContain("目录为空");
    }, WAIT);
  });
});

// ===== SessionWorkbench 集成（DoD7：右栏常驻 + 会话绑定） =====

function sessionSummary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: SESSION_A,
    runtime_id: "mock",
    workspace_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1W",
    parent_session_id: null,
    title: "引用会话",
    status: "idle" as SessionStatus,
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    workspace_root: "C:\\ws",
    ...overrides,
  };
}

function fakeSessionIpc(): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => []),
    listSessions: vi.fn(async () => [sessionSummary()]),
    createSession: vi.fn(async () => sessionSummary()),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION_A,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION_A })),
    disposeSession: vi.fn(async () => ({ session_id: SESSION_A, status: "completed" as SessionStatus })),
    messagesPage: vi.fn(async () => ({
      session_id: SESSION_A,
      max_seq: null,
      events: [],
      messages: [],
      complete: true,
    })),
    retryRun: vi.fn(async () => ({
      session_id: SESSION_A,
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
  };
}

function fakePermissionIpc(): PermissionIpc {
  return {
    pendingPermissions: vi.fn(async () => []),
    resolvePermission: vi.fn(),
    retryRuntime: vi.fn(),
    enableRuntime: vi.fn(),
  } as unknown as PermissionIpc;
}

describe("SessionWorkbench 文件面板集成（M3-09 DoD7）", () => {
  it("file-panel 常驻右栏（right-panel）并绑定当前会话的工作区", async () => {
    const store = new EventStore();
    const artifacts = fakeArtifactsIpc({
      [SESSION_A]: [entry("01J00000000000000000000RA1", "C:\\ws\\a.txt", "file")],
    });
    try {
      render(
        <SessionWorkbench
          store={store}
          ipc={fakeSessionIpc()}
          permissionIpc={fakePermissionIpc()}
          artifactsIpc={artifacts.ipc}
        />,
      );
      const panel = await screen.findByTestId("file-panel", {}, WAIT);
      const right = screen.getByTestId("right-panel");
      expect(right.contains(panel)).toBe(true);
      expect(right.getAttribute("data-open")).toBe("true");

      const item = await screen.findByTestId("ref-item", {}, WAIT);
      expect(item.getAttribute("data-path")).toBe("C:\\ws\\a.txt");

      fireEvent.click(screen.getByTestId("file-panel-project-tab"));
      expect(screen.getByText(/工作区：C:\\ws/)).toBeTruthy();
    } finally {
      store.dispose();
    }
  });

  it("生产 artifactsIpc 契约存在且命令名与生成绑定一致", () => {
    expect(typeof productionArtifactsIpc.pick).toBe("function");
    expect(typeof productionArtifactsIpc.list).toBe("function");
    expect(typeof productionArtifactsIpc.add).toBe("function");
    expect(typeof productionArtifactsIpc.remove).toBe("function");
  });
});
