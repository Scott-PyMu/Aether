/**
 * M3-08 前端集成测试：设置页工作区绑定（D14/ADR-004 决策 3；UI-UX S-05/§7.3）。
 *
 * 覆盖：
 * - `workspace-pick`（复用 `startup_pick_target`）→ `workspace-root` 回填；
 * - `workspace-apply` → `workspace_set` → `workspace-result`（data-result=ok/failed）；
 * - 失败结构化错误经 `settings-error`（data-code）；
 * - P0 口径文案（“已有会话不迁移”，D14）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SettingsPage } from "./SettingsPage";
import type { SettingsIpc } from "./settings";
import type { WorkspaceIpc } from "./workspace";

afterEach(cleanup);

const WAIT = { timeout: 5000 };

function fakeSettingsIpc(): SettingsIpc {
  return {
    get: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
    set: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
  };
}

function fakeWorkspaceIpc(overrides: Partial<WorkspaceIpc> = {}): WorkspaceIpc {
  return {
    pickDirectory: vi.fn().mockResolvedValue("C:\\Projects\\demo"),
    set: vi.fn().mockResolvedValue({
      workspace_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1W",
      name: "demo",
      root_path: "C:\\Projects\\demo",
    }),
    ...overrides,
  };
}

function renderSettings(workspace: WorkspaceIpc) {
  return render(
    <SettingsPage
      dataDir="C:\\Local\\Aether"
      ipc={fakeSettingsIpc()}
      workspace={workspace}
      onBack={() => {}}
    />,
  );
}

describe("SettingsPage 工作区绑定（M3-08）", () => {
  it("选择目录回填 workspace-root；绑定成功展示 workspace-result=ok", async () => {
    const workspace = fakeWorkspaceIpc();
    renderSettings(workspace);

    const pick = screen.getByTestId("workspace-pick");
    const apply = screen.getByTestId("workspace-apply");
    const root = screen.getByTestId("workspace-root") as HTMLInputElement;
    expect(root.value).toBe("");
    expect(apply).toHaveProperty("disabled", true);

    fireEvent.click(pick);
    await waitFor(() => {
      expect(root.value).toBe("C:\\Projects\\demo");
    }, WAIT);
    expect(workspace.pickDirectory).toHaveBeenCalledTimes(1);

    fireEvent.click(apply);
    const result = await screen.findByTestId("workspace-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain("工作区已绑定");
    expect(result.textContent).toContain("已有会话不迁移");
    expect(workspace.set).toHaveBeenCalledWith("C:\\Projects\\demo");
  });

  it("绑定失败展示结构化错误（workspace-result=failed + settings-error data-code）", async () => {
    const workspace = fakeWorkspaceIpc({
      pickDirectory: vi.fn().mockResolvedValue("D:\\Sync\\OneDrive\\ws"),
      set: vi.fn().mockRejectedValue({
        code: "path_rejected",
        message: "命中同步盘/云目录拒绝清单（OneDrive 环境变量前缀祖先）",
      }),
    });
    renderSettings(workspace);
    fireEvent.click(screen.getByTestId("workspace-pick"));
    await waitFor(() => {
      expect(
        (screen.getByTestId("workspace-root") as HTMLInputElement).value,
      ).toContain("OneDrive");
    }, WAIT);
    fireEvent.click(screen.getByTestId("workspace-apply"));

    const result = await screen.findByTestId("workspace-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("failed");
    const error = screen.getByTestId("settings-error");
    expect(error.getAttribute("data-code")).toBe("path_rejected");
    expect(error.textContent).toContain("同步盘");
  });

  it("选择取消不回填；空输入不触发绑定；P0 口径文案在案", () => {
    const workspace = fakeWorkspaceIpc({
      pickDirectory: vi.fn().mockResolvedValue(null),
    });
    renderSettings(workspace);
    fireEvent.click(screen.getByTestId("workspace-pick"));
    expect((screen.getByTestId("workspace-root") as HTMLInputElement).value).toBe("");
    expect(workspace.set).not.toHaveBeenCalled();
    const note = screen.getByTestId("settings-workspace").textContent ?? "";
    expect(note).toContain("AGENTS.md");
    expect(note).toContain("32KB");
    expect(note).toContain("已有会话不迁移");
  });

  it("手动输入路径可直接绑定（无选择器依赖）", async () => {
    const workspace = fakeWorkspaceIpc();
    renderSettings(workspace);
    const root = screen.getByTestId("workspace-root") as HTMLInputElement;
    fireEvent.change(root, { target: { value: "E:\\Aether\\workspace" } });
    fireEvent.click(screen.getByTestId("workspace-apply"));
    await waitFor(() => {
      expect(workspace.set).toHaveBeenCalledWith("E:\\Aether\\workspace");
    }, WAIT);
  });
});
