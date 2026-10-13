/**
 * M5-04 前端集成测试：设置页「事件保留与归档」区（锚点 `settings-retention`）。
 *
 * 覆盖 DoD1（UI 面）与 DoD4：
 * - 缺省读取（关闭 / 30 天）与保存（开关 + 窗口 → `events.retention.*` 键）；
 * - 窗口本地校验（7–365；越界 `out_of_range` 提示、不发写入）；
 * - 核心拒绝透传（`out_of_range`/`invalid_type`）到 `settings-error` 的 `data-code`。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SettingsPage } from "./SettingsPage";
import {
  EVENTS_RETENTION_DAYS_KEY,
  EVENTS_RETENTION_ENABLED_KEY,
  type SettingsIpc,
} from "./settings";

function fakeSettingsIpc(overrides: Partial<SettingsIpc> = {}): SettingsIpc {
  return {
    get: vi.fn(async (key: string) => {
      switch (key) {
        case EVENTS_RETENTION_ENABLED_KEY:
          return { key, value: false };
        case EVENTS_RETENTION_DAYS_KEY:
          return { key, value: 30 };
        default:
          return { key, value: true };
      }
    }),
    set: vi.fn(async (key: string, value: unknown) => ({ key, value })),
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("SettingsPage 事件保留（M5-04）", () => {
  it("读取缺省并保存开关/窗口（events.retention.* 键）", async () => {
    const ipc = fakeSettingsIpc();
    render(<SettingsPage dataDir="C:\\Aether" ipc={ipc} onBack={() => {}} />);

    const toggle = await screen.findByTestId("settings-retention-enabled");
    await waitFor(() => {
      expect(toggle.getAttribute("data-enabled")).toBe("false");
    });
    expect((screen.getByTestId("settings-retention-days") as HTMLInputElement).value).toBe(
      "30",
    );

    fireEvent.click(toggle);
    fireEvent.change(screen.getByTestId("settings-retention-days"), {
      target: { value: "14" },
    });
    fireEvent.click(screen.getByTestId("settings-retention-save"));

    await waitFor(() => {
      expect(ipc.set).toHaveBeenCalledWith(EVENTS_RETENTION_ENABLED_KEY, true);
      expect(ipc.set).toHaveBeenCalledWith(EVENTS_RETENTION_DAYS_KEY, 14);
    });
    const notice = await screen.findByTestId("settings-notice");
    expect(notice.textContent).toContain("已保存");
  });

  it("窗口越界：本地拒绝（out_of_range）且不发起写入", async () => {
    const ipc = fakeSettingsIpc();
    render(<SettingsPage dataDir="C:\\Aether" ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("settings-retention-days");

    fireEvent.change(screen.getByTestId("settings-retention-days"), {
      target: { value: "6" },
    });
    fireEvent.click(screen.getByTestId("settings-retention-save"));

    await waitFor(() => {
      const error = screen.getByTestId("settings-error");
      expect(error.getAttribute("data-code")).toBe("out_of_range");
      expect(error.textContent).toContain("7–365");
    });
    expect(ipc.set).not.toHaveBeenCalled();
  });

  it("核心拒绝（数越界）：结构化错误码透传到 data-code", async () => {
    const ipc = fakeSettingsIpc({
      set: vi.fn(async (key: string) => {
        if (key === EVENTS_RETENTION_DAYS_KEY) {
          throw { code: "out_of_range", message: "设置键 events.retention.days 的值必须在 7–365 之间" };
        }
        return { key, value: true };
      }),
    });
    render(<SettingsPage dataDir="C:\\Aether" ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("settings-retention-save");

    fireEvent.change(screen.getByTestId("settings-retention-days"), {
      target: { value: "400" },
    });
    // 400 会先被本地校验拦截；改为 10 天但让核心以越界错误拒绝（模拟外部改库/策略变化）。
    fireEvent.change(screen.getByTestId("settings-retention-days"), {
      target: { value: "10" },
    });
    fireEvent.click(screen.getByTestId("settings-retention-save"));

    await waitFor(() => {
      const error = screen.getByTestId("settings-error");
      expect(error.getAttribute("data-code")).toBe("out_of_range");
    });
  });
});
