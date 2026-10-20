/**
 * M7-08 前端集成测试：左栏工作区树（ADR-026 §2.1；UI-UX §7.3 锚点
 * `workspace-tree` / `workspace-item` / `workspace-create`）。
 *
 * 覆盖：
 * - 树渲染：工作区分组 + 计数（在途/总数）+ 折叠 + 未分配合成组「默认（数据目录）」；
 * - `workspace_create` 入口（名称 + 目录选择器/手输 → 创建 → 刷新）；
 * - 改名 / 删除二次确认 / `workspace_in_use` 结构化错误；
 * - 会话改派（`session_workspace_assign`）+ 常驻提示「权限基准不变，如需变更请新建会话」；
 * - 入口去重：树内不提供第二个全局绑定入口（无「设为默认」）。
 */
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SessionSummary } from "./session";
import type { WorkspacesIpc, WorkspacesListResult } from "./workspace";
import {
  UNASSIGNED_WORKSPACE_KEY,
  WORKSPACE_BASELINE_NOTICE,
  WorkspaceTree,
} from "./WorkspaceTree";

afterEach(cleanup);

const WAIT = { timeout: 5000 };

/** 数组下标取值（`noUncheckedIndexedAccess` 下收敛为确定元素）。 */
function at<T>(items: T[], index: number): T {
  const item = items[index];
  if (item === undefined) {
    throw new Error(`缺少第 ${index} 项（实际 ${items.length} 项）`);
  }
  return item;
}

function session(id: string, workspaceId: string | null, title = id): SessionSummary {
  return {
    id,
    runtime_id: "mock",
    workspace_id: workspaceId,
    title,
    status: "idle",
    created_at: 1,
    updated_at: 1,
  };
}

function listFixture(overrides: Partial<WorkspacesListResult> = {}): WorkspacesListResult {
  return {
    workspaces: [
      {
        workspace_id: "01J0000000000000000000W01",
        name: "工作区 A",
        root_path: "C:\\ws\\a",
        memory_files: [],
        active_sessions: 1,
        total_sessions: 2,
        created_at: 1,
        updated_at: 2,
      },
      {
        workspace_id: "01J0000000000000000000W02",
        name: "工作区 B",
        root_path: "C:\\ws\\b",
        memory_files: [],
        active_sessions: 0,
        total_sessions: 0,
        created_at: 1,
        updated_at: 1,
      },
    ],
    unassigned: {
      name: "默认（数据目录）",
      root_path: "C:\\Local\\Aether",
      active_sessions: 0,
      total_sessions: 1,
    },
    ...overrides,
  };
}

function fakeIpc(overrides: Partial<WorkspacesIpc> = {}): WorkspacesIpc {
  return {
    list: vi.fn().mockResolvedValue(listFixture()),
    create: vi.fn().mockResolvedValue({
      workspace_id: "01J0000000000000000000W03",
      name: "新工作区",
      root_path: "C:\\ws\\c",
      memory_files: [],
      active_sessions: 0,
      total_sessions: 0,
      created_at: 1,
      updated_at: 1,
    }),
    update: vi.fn().mockResolvedValue({
      workspace_id: "01J0000000000000000000W01",
      name: "改名后",
      root_path: "C:\\ws\\a",
      memory_files: [],
      active_sessions: 1,
      total_sessions: 2,
      created_at: 1,
      updated_at: 3,
    }),
    remove: vi.fn().mockResolvedValue({
      workspace_id: "01J0000000000000000000W02",
      deleted: true,
      disk_untouched: true,
    }),
    assignSession: vi.fn().mockResolvedValue({
      session_id: "01J00000000000000000000S1",
      workspace_id: "01J0000000000000000000W02",
      previous_workspace_id: "01J0000000000000000000W01",
      changed: true,
      baseline_unchanged: true,
      baseline_root: "C:\\ws\\a",
      memory_injection_switched: true,
      notice: WORKSPACE_BASELINE_NOTICE,
    }),
    pickDirectory: vi.fn().mockResolvedValue("C:\\ws\\c"),
    ...overrides,
  };
}

function renderTree(
  ipc: WorkspacesIpc,
  options: {
    sessions?: SessionSummary[];
    activeSessionId?: string | null;
    onAssigned?: () => void | Promise<void>;
    onOpenSession?: (id: string) => void;
  } = {},
) {
  return render(
    <WorkspaceTree
      ipc={ipc}
      sessions={
        options.sessions ?? [
          session("01J00000000000000000000S1", "01J0000000000000000000W01", "会话 A1"),
          session("01J00000000000000000000S2", "01J0000000000000000000W01", "会话 A2"),
          session("01J00000000000000000000S3", null, "会话 D1"),
        ]
      }
      activeSessionId={
        options.activeSessionId === undefined ? "01J00000000000000000000S1" : options.activeSessionId
      }
      onAssigned={options.onAssigned}
      onOpenSession={options.onOpenSession}
    />,
  );
}

describe("WorkspaceTree（M7-08）", () => {
  it("渲染工作区节点/计数 + 未分配合成组，且不提供第二个全局绑定入口", async () => {
    const ipc = fakeIpc();
    renderTree(ipc);

    const items = await screen.findAllByTestId("workspace-item", {}, WAIT);
    expect(items.map((item) => item.getAttribute("data-workspace-id"))).toEqual([
      "01J0000000000000000000W01",
      "01J0000000000000000000W02",
      UNASSIGNED_WORKSPACE_KEY,
    ]);
    const itemA = at(items, 0);
    expect(within(itemA).getByTestId("workspace-item-count").getAttribute("data-active")).toBe(
      "1",
    );
    expect(within(itemA).getByTestId("workspace-item-count").getAttribute("data-total")).toBe(
      "2",
    );
    expect(within(itemA).getByTestId("workspace-item-count").textContent).toBe("1/2");
    // 合成组名称与计数（不建 workspaces 行）；合成组无管理动作。
    const defaultGroup = at(items, 2);
    expect(defaultGroup.textContent).toContain("默认（数据目录）");
    expect(within(defaultGroup).queryByTestId("workspace-rename")).toBeNull();
    expect(within(defaultGroup).queryByTestId("workspace-delete")).toBeNull();
    // 常驻提示 + 无全局绑定入口（「设为默认」/「绑定工作区」）。
    expect(screen.getByTestId("workspace-assign-notice").textContent).toBe(
      WORKSPACE_BASELINE_NOTICE,
    );
    expect(screen.queryByText(/设为默认/)).toBeNull();
    expect(screen.queryByText(/绑定工作区/)).toBeNull();
  });

  it("展开/折叠工作区节点；点击会话叶子打开", async () => {
    const open = vi.fn();
    const ipc = fakeIpc();
    renderTree(ipc, { onOpenSession: open });
    expect(await screen.findByText("会话 A1", {}, WAIT)).toBeTruthy();
    fireEvent.click(screen.getByText("会话 A1"));
    expect(open).toHaveBeenCalledWith("01J00000000000000000000S1");

    const itemA = at(await screen.findAllByTestId("workspace-item", {}, WAIT), 0);
    const toggle = within(itemA).getByTestId("workspace-item-toggle");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("会话 A1")).toBeNull();
    fireEvent.click(toggle);
    expect(await screen.findByText("会话 A1", {}, WAIT)).toBeTruthy();
  });

  it("创建工作区：目录选择器回填 + 提交调用 workspace_create 并刷新", async () => {
    const ipc = fakeIpc();
    renderTree(ipc);
    await screen.findAllByTestId("workspace-item", {}, WAIT);

    fireEvent.click(screen.getByTestId("workspace-create"));
    const form = await screen.findByTestId("workspace-create-form", {}, WAIT);
    const name = screen.getByTestId("workspace-name-input") as HTMLInputElement;
    fireEvent.change(name, { target: { value: "  新工作区  " } });
    fireEvent.click(within(form).getByTestId("workspace-root-pick"));
    await waitFor(() => {
      expect((screen.getByTestId("workspace-root-input") as HTMLInputElement).value).toBe(
        "C:\\ws\\c",
      );
    }, WAIT);
    expect(ipc.pickDirectory).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByTestId("workspace-create-submit"));
    await waitFor(() => {
      expect(ipc.create).toHaveBeenCalledWith({ name: "新工作区", root_path: "C:\\ws\\c" });
    }, WAIT);
    await waitFor(() => {
      expect(screen.queryByTestId("workspace-create-form")).toBeNull();
    }, WAIT);
    expect(ipc.list).toHaveBeenCalledTimes(2);
  });

  it("创建失败展示结构化错误（data-code）", async () => {
    const ipc = fakeIpc({
      create: vi.fn().mockRejectedValue({
        code: "invalid_value",
        message: "工作区根路径已存在；请直接在左栏树中使用既有工作区",
      }),
    });
    renderTree(ipc);
    await screen.findAllByTestId("workspace-item", {}, WAIT);
    fireEvent.click(screen.getByTestId("workspace-create"));
    fireEvent.change(screen.getByTestId("workspace-name-input"), {
      target: { value: "已存在" },
    });
    fireEvent.change(screen.getByTestId("workspace-root-input"), {
      target: { value: "C:\\ws\\a" },
    });
    fireEvent.click(screen.getByTestId("workspace-create-submit"));
    const error = await screen.findByTestId("workspace-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("已存在");
  });

  it("改名：输入新名称并保存（workspace_update）", async () => {
    const ipc = fakeIpc();
    renderTree(ipc);
    const itemA = at(await screen.findAllByTestId("workspace-item", {}, WAIT), 0);
    fireEvent.click(within(itemA).getByTestId("workspace-rename"));
    const input = within(itemA).getByTestId("workspace-rename-input") as HTMLInputElement;
    expect(input.value).toBe("工作区 A");
    fireEvent.change(input, { target: { value: "改名后" } });
    fireEvent.click(within(itemA).getByTestId("workspace-rename-save"));
    await waitFor(() => {
      expect(ipc.update).toHaveBeenCalledWith("01J0000000000000000000W01", "改名后");
    }, WAIT);
    await waitFor(() => {
      expect(within(itemA).queryByTestId("workspace-rename-input")).toBeNull();
    }, WAIT);
  });

  it("删除：二次确认后调用；存在引用 → workspace_in_use 提示", async () => {
    const remove = vi
      .fn()
      .mockRejectedValueOnce({
        code: "workspace_in_use",
        message: "仍有 2 个会话引用该工作区；请先改派会话再删除",
      })
      .mockResolvedValueOnce({
        workspace_id: "01J0000000000000000000W01",
        deleted: true,
        disk_untouched: true,
      });
    const ipc = fakeIpc({ remove });
    renderTree(ipc);
    const itemA = at(await screen.findAllByTestId("workspace-item", {}, WAIT), 0);
    fireEvent.click(within(itemA).getByTestId("workspace-delete"));
    expect(within(itemA).getByTestId("workspace-delete-confirm-box").textContent).toContain(
      "不删除磁盘文件",
    );
    fireEvent.click(within(itemA).getByTestId("workspace-delete-confirm"));
    const error = await screen.findByTestId("workspace-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("workspace_in_use");
    expect(error.textContent).toContain("仍有 2 个会话引用");
    expect(remove).toHaveBeenCalledWith("01J0000000000000000000W01");

    // 重试成功 → 关闭确认框并刷新。
    fireEvent.click(within(itemA).getByTestId("workspace-delete-confirm"));
    await waitFor(() => {
      expect(within(itemA).queryByTestId("workspace-delete-confirm-box")).toBeNull();
    }, WAIT);
    expect(ipc.list).toHaveBeenCalledTimes(2);
  });

  it("改派：调用 session_workspace_assign + 提示基准不变 + 触发刷新", async () => {
    const onAssigned = vi.fn();
    const ipc = fakeIpc();
    renderTree(ipc, { onAssigned });
    const items = await screen.findAllByTestId("workspace-item", {}, WAIT);
    const itemB = at(items, 1);
    const assign = within(itemB).getByTestId("workspace-assign") as HTMLButtonElement;
    expect(assign.disabled).toBe(false);
    expect(assign.getAttribute("data-session-id")).toBe("01J00000000000000000000S1");
    fireEvent.click(assign);
    await waitFor(() => {
      expect(ipc.assignSession).toHaveBeenCalledWith(
        "01J00000000000000000000S1",
        "01J0000000000000000000W02",
      );
    }, WAIT);
    const result = await screen.findByTestId("workspace-assign-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain(WORKSPACE_BASELINE_NOTICE);
    expect(result.textContent).toContain("C:\\ws\\a");
    expect(onAssigned).toHaveBeenCalledTimes(1);
    // 已在该工作区的节点改派按钮禁用。
    const itemA = at(items, 0);
    expect((within(itemA).getByTestId("workspace-assign") as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("无语义会话时改派按钮禁用（不提供空操作）", async () => {
    const ipc = fakeIpc();
    renderTree(ipc, { activeSessionId: null });
    const items = await screen.findAllByTestId("workspace-item", {}, WAIT);
    for (const item of items.slice(0, 2)) {
      expect((within(item).getByTestId("workspace-assign") as HTMLButtonElement).disabled).toBe(
        true,
      );
    }
  });

  it("改派失败：结构化错误 + data-result=failed", async () => {
    const ipc = fakeIpc({
      assignSession: vi.fn().mockRejectedValue({
        code: "invalid_value",
        message: "session_busy：会话在途，不可改派工作区",
      }),
    });
    renderTree(ipc);
    const items = await screen.findAllByTestId("workspace-item", {}, WAIT);
    fireEvent.click(within(at(items, 1)).getByTestId("workspace-assign"));
    const result = await screen.findByTestId("workspace-assign-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("failed");
    const error = screen.getByTestId("workspace-error");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("session_busy");
  });
});
