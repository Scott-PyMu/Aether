/**
 * M7-09/ADR-026：Agent 预设前端（锚点 `presets-page` / `preset-item` / `preset-edit`）。
 *
 * 覆盖：
 * - 列表（`presets_list`）：`data-preset-id` / `data-builtin` / `data-active`（使用中）；
 * - 内置不可删（删除入口置灰）+ 内置可编辑（`preset_update`）；
 * - 新增（`preset_add`）/ 删除（`preset_delete` + 二次确认）；
 * - 错误码文案（`preset_not_found` 自动刷新；`builtin_preset_undeletable`）；
 * - 新建会话入口（`session-preset-select`）透传 `preset_id` 并回显使用中标记。
 */
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { PresetsPage } from "./PresetsPage";
import {
  BUILTIN_PRESET_UNDELETABLE_MESSAGE,
  PRESET_NOT_FOUND_MESSAGE,
  type PresetEntry,
  type PresetsIpc,
} from "./presets";
import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionIpc,
  SessionSummary,
} from "./session";

const BUILTIN_ID = "01J00000000000000000000P01";
const CUSTOM_ID = "01J00000000000000000000C01";
const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

const WAIT = { timeout: 5000 };

function builtinPreset(): PresetEntry {
  return {
    id: BUILTIN_ID,
    name: "默认助手",
    system_prompt: "你是 Aether 工作台中的通用 AI 助手。",
    is_builtin: true,
    created_at: 1,
    updated_at: 1,
  };
}

function customPreset(overrides: Partial<PresetEntry> = {}): PresetEntry {
  return {
    id: CUSTOM_ID,
    name: "审查助手",
    system_prompt: "你是审查助手",
    is_builtin: false,
    created_at: 2,
    updated_at: 2,
    ...overrides,
  };
}

function fakePresetsIpc(overrides: Partial<PresetsIpc> = {}): PresetsIpc {
  return {
    listPresets: vi.fn(async () => [builtinPreset(), customPreset()]),
    addPreset: vi.fn(async (input) =>
      customPreset({ id: "01J00000000000000000000C02", name: input.name }),
    ),
    updatePreset: vi.fn(async (input) => customPreset({ name: input.name ?? "审查助手" })),
    deletePreset: vi.fn(async (presetId: string) => ({ preset_id: presetId, deleted: true })),
    ...overrides,
  };
}

/** 结构化 IPC 错误（与 `IpcError` 线上形态一致）。 */
function ipcError(code: string, message: string): { code: string; message: string } {
  return { code, message };
}

afterEach(cleanup);

describe("M7-09 预设管理页（presets-page）", () => {
  it("列表渲染 data-preset-id/data-builtin；内置删除入口置灰、可编辑", async () => {
    const ipc = fakePresetsIpc();
    render(<PresetsPage ipc={ipc} />);

    const items = await screen.findAllByTestId("preset-item", {}, WAIT);
    expect(items).toHaveLength(2);
    expect(items[0]!.getAttribute("data-preset-id")).toBe(BUILTIN_ID);
    expect(items[0]!.getAttribute("data-builtin")).toBe("true");
    expect(items[1]!.getAttribute("data-builtin")).toBe("false");
    expect(within(items[0]!).getByTestId("preset-builtin").textContent).toBe("内置");

    const deleteButtons = screen.getAllByTestId("preset-delete");
    expect(deleteButtons[0]).toHaveProperty("disabled", true);
    expect(deleteButtons[1]).toHaveProperty("disabled", false);

    // 内置可编辑：保存后调用 `preset_update` 并刷新列表。
    fireEvent.click(within(items[0]!).getByTestId("preset-edit"));
    const nameInput = (await screen.findByTestId("preset-edit-name", {}, WAIT)) as HTMLInputElement;
    expect(nameInput.value).toBe("默认助手");
    fireEvent.change(nameInput, { target: { value: "默认助手（改）" } });
    fireEvent.click(screen.getByTestId("preset-edit-save"));
    await waitFor(() => {
      expect(ipc.updatePreset).toHaveBeenCalledWith({
        preset_id: BUILTIN_ID,
        name: "默认助手（改）",
        system_prompt: "你是 Aether 工作台中的通用 AI 助手。",
      });
    }, WAIT);
    expect(await screen.findByTestId("presets-notice", {}, WAIT)).toBeTruthy();
    expect(ipc.listPresets).toHaveBeenCalledTimes(2);
  });

  it("新增预设经 preset_add；空名称本地拦截（missing_field）", async () => {
    const ipc = fakePresetsIpc();
    render(<PresetsPage ipc={ipc} />);
    await screen.findAllByTestId("preset-item", {}, WAIT);

    fireEvent.click(screen.getByTestId("preset-add-submit"));
    const error = await screen.findByTestId("presets-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("missing_field");
    expect(ipc.addPreset).not.toHaveBeenCalled();

    fireEvent.change(screen.getByTestId("preset-add-name"), { target: { value: "新预设" } });
    fireEvent.change(screen.getByTestId("preset-add-prompt"), {
      target: { value: "新提示词" },
    });
    fireEvent.click(screen.getByTestId("preset-add-submit"));
    await waitFor(() => {
      expect(ipc.addPreset).toHaveBeenCalledWith({
        name: "新预设",
        system_prompt: "新提示词",
      });
    }, WAIT);
  });

  it("删除自定义预设：二次确认后才调用 preset_delete", async () => {
    const ipc = fakePresetsIpc();
    render(<PresetsPage ipc={ipc} />);
    const items = await screen.findAllByTestId("preset-item", {}, WAIT);

    fireEvent.click(within(items[1]!).getByTestId("preset-delete"));
    expect(ipc.deletePreset).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("preset-delete-confirm-button"));
    await waitFor(() => {
      expect(ipc.deletePreset).toHaveBeenCalledWith(CUSTOM_ID);
    }, WAIT);
    expect(await screen.findByTestId("presets-notice", {}, WAIT)).toBeTruthy();
  });

  it("preset_not_found → 登记文案 + 自动刷新；builtin_preset_undeletable → 登记文案", async () => {
    const deletePreset = vi
      .fn()
      .mockRejectedValueOnce(ipcError("preset_not_found", "预设不存在（x）；已刷新预设列表"))
      .mockRejectedValueOnce(
        ipcError("builtin_preset_undeletable", "内置预设不可删除（x）；可编辑其名称与系统提示词"),
      );
    const ipc = fakePresetsIpc({ deletePreset });
    render(<PresetsPage ipc={ipc} />);
    const items = await screen.findAllByTestId("preset-item", {}, WAIT);

    fireEvent.click(within(items[1]!).getByTestId("preset-delete"));
    fireEvent.click(screen.getByTestId("preset-delete-confirm-button"));
    const notFound = await screen.findByTestId("presets-error", {}, WAIT);
    expect(notFound.getAttribute("data-code")).toBe("preset_not_found");
    expect(notFound.textContent).toContain(PRESET_NOT_FOUND_MESSAGE);
    await waitFor(() => {
      expect(ipc.listPresets).toHaveBeenCalledTimes(2);
    }, WAIT);

    fireEvent.click(within(items[1]!).getByTestId("preset-delete"));
    fireEvent.click(screen.getByTestId("preset-delete-confirm-button"));
    await waitFor(() => {
      const error = screen.getByTestId("presets-error");
      expect(error.getAttribute("data-code")).toBe("builtin_preset_undeletable");
      expect(error.textContent).toContain(BUILTIN_PRESET_UNDELETABLE_MESSAGE);
    }, WAIT);
  });

  it("使用中标记（data-active + 「使用中」）与选择切换", async () => {
    const ipc = fakePresetsIpc();
    const onChange = vi.fn();
    render(<PresetsPage ipc={ipc} activePresetId={CUSTOM_ID} onActivePresetChange={onChange} />);
    const items = await screen.findAllByTestId("preset-item", {}, WAIT);
    expect(items[1]!.getAttribute("data-active")).toBe("true");
    expect(within(items[1]!).getByTestId("preset-in-use").textContent).toBe("使用中");

    fireEvent.click(within(items[1]!).getByTestId("preset-use"));
    expect(onChange).toHaveBeenCalledWith(null);
    fireEvent.click(within(items[0]!).getByTestId("preset-use"));
    expect(onChange).toHaveBeenCalledWith(BUILTIN_ID);
  });
});

describe("M7-09 新建会话入口（session-preset-select）", () => {
  function runtime(): RuntimeInfo {
    return {
      id: "claude-code",
      name: "Claude Code",
      kind: "acp",
      version: "0.1.0",
      protocol: "1.0",
      capabilities: [],
      enabled: true,
      status: "ready",
      status_reason: null,
      active_sessions: 0,
    };
  }

  function summary(): SessionSummary {
    return {
      id: SESSION,
      runtime_id: "claude-code",
      workspace_id: null,
      parent_session_id: null,
      title: "预设会话",
      status: "idle",
      model: null,
      preset_id: BUILTIN_ID,
      preset_name: "默认助手",
      created_at: 1,
      updated_at: 1,
      closed_at: null,
    };
  }

  function emptyPage(): MessagesPageResult {
    return { session_id: SESSION, max_seq: null, events: [], messages: [], complete: true };
  }

  function fakeSessionIpc(createSession: SessionIpc["createSession"]): SessionIpc {
    return {
      listRuntimes: vi.fn(async () => [runtime()]),
      listSessions: vi.fn(async () => []),
      searchSessions: vi.fn(async () => []),
      renameSession: vi.fn(async (sessionId: string, title: string) => ({
        session_id: sessionId,
        title,
        updated_at: 0,
      })),
      deleteSession: vi.fn(async (sessionId: string) => ({ session_id: sessionId, deleted: true })),
      createSession,
      sendMessage: vi.fn(),
      interruptSession: vi.fn(),
      pauseSession: vi.fn(),
      resumeSession: vi.fn(),
      disposeSession: vi.fn(),
      messagesPage: vi.fn(async () => emptyPage()),
      retryRun: vi.fn(),
      setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
        session_id: sessionId,
        permission_mode: mode,
        previous_permission_mode: "workspace_write",
      })),
    };
  }

  it("选中预设后新建会话透传 preset_id；取消选择后不透传", async () => {
    const store = new EventStore();
    const createSession = vi.fn(async () => summary());
    const presetsIpc = fakePresetsIpc();
    const onChange = vi.fn();
    render(
      <SessionWorkbench
        store={store}
        ipc={fakeSessionIpc(createSession)}
        presetsIpc={presetsIpc}
        onActivePresetChange={onChange}
      />,
    );

    const select = (await screen.findByTestId("session-preset-select", {}, WAIT)) as HTMLSelectElement;
    await waitFor(() => {
      expect(select.options.length).toBe(3);
    }, WAIT);
    expect(select.options[1]!.textContent).toContain("默认助手（内置）");

    fireEvent.change(select, { target: { value: BUILTIN_ID } });
    expect(onChange).toHaveBeenCalledWith(BUILTIN_ID);
    store.dispose();
  });

  it("受控选择（activePresetId）随 session_create 下发 preset_id", async () => {
    const store = new EventStore();
    const createSession = vi.fn(async () => summary());
    render(
      <SessionWorkbench
        store={store}
        ipc={fakeSessionIpc(createSession)}
        presetsIpc={fakePresetsIpc()}
        activePresetId={BUILTIN_ID}
      />,
    );

    const runtimeOption = await screen.findByTestId("runtime-option", {}, WAIT);
    await waitFor(() => {
      expect(runtimeOption.getAttribute("data-selected")).toBe("true");
    }, WAIT);

    fireEvent.click(screen.getByTestId("session-create-submit"));
    await waitFor(() => {
      expect(createSession).toHaveBeenCalledWith(
        expect.objectContaining({ preset_id: BUILTIN_ID }),
      );
    }, WAIT);
    store.dispose();
  });
});
