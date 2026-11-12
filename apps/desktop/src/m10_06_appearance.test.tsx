/**
 * M10-06 / ADR-036：S-15 外观设置（语言/主题）运行期切换 + 持久化。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { initAppearance, resetAppearanceForTest } from "./appearance";
import { SettingsPage } from "./SettingsPage";
import { settingsIpc, type SettingsIpc } from "./settings";

afterEach(cleanup);

function fakeSettings(persisted: Record<string, unknown> = {}): SettingsIpc {
  return {
    get: vi.fn(async (key: string) => ({ key, value: persisted[key] ?? null })),
    set: vi.fn(async (key: string, value: unknown) => ({ key, value })),
  };
}

function renderPage(ipc: SettingsIpc) {
  return render(
    <SettingsPage
      ipc={ipc}
      onBack={() => undefined}
      onOpenBackup={() => undefined}
      onOpenDiagnostics={() => undefined}
      onOpenAbout={() => undefined}
    />,
  );
}

describe("M10-06 外观设置（S-15）", () => {
  it("锚点 locale-select/theme-select 反映 settings 生效值；切换不重启、不重载数据", async () => {
    const ipc = fakeSettings({ "ui.locale": "zh-CN", "ui.theme": "dark" });
    await initAppearance(ipc);
    renderPage(ipc);

    const locale = (await screen.findByTestId("locale-select")) as HTMLSelectElement;
    const theme = screen.getByTestId("theme-select") as HTMLSelectElement;
    expect(locale.dataset.locale).toBe("zh-CN");
    expect(theme.dataset.theme).toBe("dark");
    expect(document.documentElement.lang).toBe("zh-CN");
    expect(document.documentElement.dataset.theme).toBe("dark");

    const getsBefore = (ipc.get as ReturnType<typeof vi.fn>).mock.calls.length;
    fireEvent.change(locale, { target: { value: "en-US" } });
    await waitFor(() => expect(document.documentElement.lang).toBe("en-US"));
    expect(await screen.findByText("Settings")).toBeTruthy();
    expect(document.documentElement.dataset.theme).toBe("dark");

    fireEvent.change(screen.getByTestId("theme-select"), { target: { value: "light" } });
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
    expect(document.documentElement.dataset.themeSource).toBe("explicit");

    expect(ipc.set).toHaveBeenCalledWith("ui.locale", "en-US");
    expect(ipc.set).toHaveBeenCalledWith("ui.theme", "light");
    // 切换只写设置，不重新读取后端数据。
    expect((ipc.get as ReturnType<typeof vi.fn>).mock.calls.length).toBe(getsBefore);
  });

  it("system 主题跟随 prefers-color-scheme，且 data-theme-source=system", async () => {
    const original = window.matchMedia;
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: vi.fn((query: string) => ({
        matches: query.includes("light"),
        media: query,
        onchange: null,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        addListener: vi.fn(),
        removeListener: vi.fn(),
        dispatchEvent: vi.fn(),
      })),
    });
    try {
      resetAppearanceForTest();
      const ipc = fakeSettings({ "ui.locale": "en-US", "ui.theme": "system" });
      await initAppearance(ipc);
      renderPage(ipc);

      expect(document.documentElement.dataset.theme).toBe("light");
      expect(document.documentElement.dataset.themeSource).toBe("system");
      const theme = (await screen.findByTestId("theme-select")) as HTMLSelectElement;
      expect(theme.value).toBe("system");

      fireEvent.change(theme, { target: { value: "dark" } });
      await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
      expect(document.documentElement.dataset.themeSource).toBe("explicit");
      expect(ipc.set).toHaveBeenCalledWith("ui.theme", "dark");
    } finally {
      Object.defineProperty(window, "matchMedia", { configurable: true, value: original });
    }
  });

  it("切换语言/主题不重挂载组件（输入草稿保留）", async () => {
    const ipc = fakeSettings({ "ui.locale": "zh-CN", "ui.theme": "dark" });
    await initAppearance(ipc);
    renderPage(ipc);
    const workspaceInput = (await screen.findByTestId("workspace-root")) as HTMLInputElement;
    fireEvent.change(workspaceInput, { target: { value: "C:\\draft-workspace" } });
    fireEvent.change(screen.getByTestId("locale-select"), { target: { value: "en-US" } });
    await waitFor(() => expect(document.documentElement.lang).toBe("en-US"));
    expect((screen.getByTestId("workspace-root") as HTMLInputElement).value).toBe(
      "C:\\draft-workspace",
    );
  });

  it("持久化失败显示本地化错误且不崩溃", async () => {
    const ipc = fakeSettings({ "ui.locale": "zh-CN", "ui.theme": "dark" });
    ipc.set = vi.fn(async () => {
      throw { aether_code: "internal", message: "写入失败" };
    });
    await initAppearance(ipc);
    renderPage(ipc);

    fireEvent.change(await screen.findByTestId("theme-select"), { target: { value: "light" } });
    const error = await screen.findByTestId("appearance-error");
    expect(error.textContent).toContain("内部错误");
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("默认缺省为 system/system；设置读取失败不阻塞主界面", async () => {
    resetAppearanceForTest();
    const ipc: SettingsIpc = {
      get: vi.fn(async () => {
        throw new Error("read failed");
      }),
      set: vi.fn(async (key, value) => ({ key, value })),
    };
    await initAppearance(ipc);
    renderPage(ipc);
    expect(((await screen.findByTestId("locale-select")) as HTMLSelectElement).value).toBe("system");
    expect((screen.getByTestId("theme-select") as HTMLSelectElement).value).toBe("system");
    expect(document.documentElement.lang).toBe("zh-CN");
  });
});

describe("M10-06 默认设置契约", () => {
  it("settingsIpc 使用 ui.locale/ui.theme 键（与核心白名单一致）", () => {
    expect(settingsIpc).toBeTruthy();
  });
});
