/**
 * M5-03 前端集成测试：加密备份开关（DoD5 前端面）。
 *
 * 覆盖：
 * - S-05 设置页：`settings-backup-encryption` 读取（缺省开启）与即时持久化
 *   （`settings_set(BACKUP_ENCRYPTION_ENABLED_KEY, …)`）；关闭提示含明文风险口径。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SettingsPage } from "./SettingsPage";
import { BACKUP_ENCRYPTION_ENABLED_KEY, type SettingsIpc } from "./settings";

const WAIT = { timeout: 5000 };

function fakeSettingsIpc(values: Partial<Record<string, unknown>> = {}): SettingsIpc {
  const store: Record<string, unknown> = { ...values };
  return {
    get: vi
      .fn()
      .mockImplementation(async (key: string) => ({ key, value: store[key] ?? null })),
    set: vi.fn().mockImplementation(async (key: string, value: unknown) => {
      store[key] = value;
      return { key, value };
    }),
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("SettingsPage 加密备份开关（M5-03）", () => {
  it("缺省开启（设置缺省语义）并可即时关闭（settings_set）", async () => {
    const ipc = fakeSettingsIpc();
    render(<SettingsPage dataDir="C:\\Local\\Aether" ipc={ipc} onBack={() => {}} />);

    const toggle = await screen.findByTestId("settings-backup-encryption", {}, WAIT);
    await waitFor(() => {
      expect(toggle).toHaveProperty("checked", true);
    }, WAIT);
    expect(toggle.getAttribute("data-encryption")).toBe("on");

    fireEvent.click(toggle);
    await waitFor(() => {
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_ENCRYPTION_ENABLED_KEY, false);
    }, WAIT);
    await waitFor(() => {
      expect(toggle).toHaveProperty("checked", false);
    }, WAIT);
    expect(screen.getByTestId("settings-notice").textContent).toContain(
      "已关闭加密备份",
    );
    expect(screen.getByTestId("settings-notice").textContent).toContain("明文");
  });

  it("已关闭状态回显（settings_get = false）→ 开关关闭并可重新开启", async () => {
    const ipc = fakeSettingsIpc({ [BACKUP_ENCRYPTION_ENABLED_KEY]: false });
    render(<SettingsPage dataDir="C:\\Local\\Aether" ipc={ipc} onBack={() => {}} />);

    const toggle = await screen.findByTestId("settings-backup-encryption", {}, WAIT);
    await waitFor(() => {
      expect(toggle).toHaveProperty("checked", false);
    }, WAIT);
    expect(toggle.getAttribute("data-encryption")).toBe("off");

    fireEvent.click(toggle);
    await waitFor(() => {
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_ENCRYPTION_ENABLED_KEY, true);
    }, WAIT);
    expect(screen.getByTestId("settings-notice").textContent).toContain(
      "已开启加密备份",
    );
  });
});
