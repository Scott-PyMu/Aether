/**
 * M7-04/ADR-018 附录 A-6：`ui.layout` 布局模型单测。
 *
 * 覆盖解析（缺省/恢复/损坏回退/引用归位）、持久化读写失败路径，
 * 以及标签/分屏纯函数（打开/聚焦/关闭/分屏/裁剪）。
 */
import { describe, expect, it, vi } from "vitest";

import {
  activateTab,
  clampLayout,
  closeTab,
  defaultLayout,
  LAYOUT_VERSION,
  loadLayout,
  MAX_TABS,
  openTab,
  parseLayoutValue,
  saveLayout,
  setSplit,
  type WorkbenchLayout,
} from "./layout";
import { UI_LAYOUT_KEY, type SettingsIpc } from "./settings";

const A = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const B = "01J8ZQ5R0N7W9Y8X6V4T2S0K1B";
const C = "01J8ZQ5R0N7W9Y8X6V4T2S0K1C";

function layout(overrides: Partial<WorkbenchLayout> = {}): WorkbenchLayout {
  return { version: LAYOUT_VERSION, tabs: [A, B], active: A, split: null, ...overrides };
}

function fakeIpc(overrides: Partial<SettingsIpc> = {}): SettingsIpc {
  return {
    get: vi.fn(async () => ({ key: UI_LAYOUT_KEY, value: "{}" })),
    set: vi.fn(async (key: string, value: unknown) => ({ key, value })),
    ...overrides,
  };
}

describe("parseLayoutValue（DoD3：损坏回退默认布局）", () => {
  it("缺省 {} / null → 首次运行（无持久化，空布局）", () => {
    for (const raw of ["{}", null, undefined]) {
      const result = parseLayoutValue(raw);
      expect(result.persisted).toBe(false);
      expect(result.corrupted).toBe(false);
      expect(result.layout).toEqual(defaultLayout());
    }
  });

  it("合法布局逐字段恢复（标签顺序/聚焦/分屏）", () => {
    const result = parseLayoutValue(
      JSON.stringify({ version: LAYOUT_VERSION, tabs: [B, A], active: B, split: A }),
    );
    expect(result).toEqual({
      layout: { version: LAYOUT_VERSION, tabs: [B, A], active: B, split: A },
      persisted: true,
      corrupted: false,
    });
  });

  it("重复标签去重；active/split 悬空 → 引用归位（不视为损坏）", () => {
    const result = parseLayoutValue(
      JSON.stringify({ version: 1, tabs: [A, A, B], active: "ghost", split: A }),
    );
    expect(result.corrupted).toBe(false);
    expect(result.layout.tabs).toEqual([A, B]);
    expect(result.layout.active).toBe(A);
    // split 与 active 相同 → 归位为 null。
    expect(result.layout.split).toBeNull();
  });

  it.each([
    ["非字符串值", 42],
    ["非法 JSON", "{oops"],
    ["非对象", "[]"],
    ["版本不符", JSON.stringify({ version: 2, tabs: [], active: null, split: null })],
    ["tabs 非数组", JSON.stringify({ version: 1, tabs: "nope", active: null, split: null })],
    [
      "tabs 含非字符串",
      JSON.stringify({ version: 1, tabs: [1], active: null, split: null }),
    ],
    [
      "active 类型非法",
      JSON.stringify({ version: 1, tabs: [A], active: 7, split: null }),
    ],
    [
      "split 类型非法",
      JSON.stringify({ version: 1, tabs: [A], active: A, split: 7 }),
    ],
    [
      "标签超上限",
      JSON.stringify({
        version: 1,
        tabs: Array.from({ length: MAX_TABS + 1 }, (_, index) => `id-${index}`),
        active: null,
        split: null,
      }),
    ],
  ])("损坏输入（%s）→ 默认布局 + corrupted=true", (_label, raw) => {
    const result = parseLayoutValue(raw);
    expect(result.corrupted).toBe(true);
    expect(result.persisted).toBe(true);
    expect(result.layout).toEqual(defaultLayout());
  });
});

describe("loadLayout / saveLayout（settings 读写失败路径）", () => {
  it("loadLayout 读取 ui.layout 并解析", async () => {
    const ipc = fakeIpc({
      get: vi.fn(async () => ({
        key: UI_LAYOUT_KEY,
        value: JSON.stringify({ version: 1, tabs: [A], active: A, split: null }),
      })),
    });
    const result = await loadLayout(ipc);
    expect(ipc.get).toHaveBeenCalledWith(UI_LAYOUT_KEY);
    expect(result.layout.tabs).toEqual([A]);
  });

  it("loadLayout 读取失败 → 首次运行默认布局（不抛）", async () => {
    const ipc = fakeIpc({
      get: vi.fn(async () => {
        throw { code: "core_not_ready", message: "设置不可用" };
      }),
    });
    const result = await loadLayout(ipc);
    expect(result.persisted).toBe(false);
    expect(result.layout).toEqual(defaultLayout());
  });

  it("saveLayout 写入 JSON 字符串；失败返回 false（不抛）", async () => {
    const set = vi.fn(async (key: string, value: unknown) => ({ key, value }));
    const ipc = fakeIpc({ set });
    const target = layout({ split: B });
    expect(await saveLayout(ipc, target)).toBe(true);
    expect(set).toHaveBeenCalledWith(UI_LAYOUT_KEY, JSON.stringify(target));

    const failing = fakeIpc({
      set: vi.fn(async () => {
        throw { code: "internal", message: "写入失败" };
      }),
    });
    expect(await saveLayout(failing, target)).toBe(false);
  });
});

describe("布局纯函数（标签/分屏）", () => {
  it("openTab：追加并聚焦；已聚焦时原样返回", () => {
    const opened = openTab(defaultLayout(), A);
    expect(opened.tabs).toEqual([A]);
    expect(opened.active).toBe(A);
    const withB = openTab(opened, B);
    expect(withB.tabs).toEqual([A, B]);
    expect(withB.active).toBe(B);
    expect(openTab(withB, B)).toBe(withB);
  });

  it("openTab：聚焦分屏会话 → 两窗格对调；超过上限丢弃最旧标签", () => {
    const state = layout({ active: A, split: B });
    const swapped = openTab(state, B);
    expect(swapped.active).toBe(B);
    expect(swapped.split).toBe(A);

    let capped = defaultLayout();
    for (let index = 0; index < MAX_TABS + 3; index += 1) {
      capped = openTab(capped, `session-${index}`);
    }
    expect(capped.tabs).toHaveLength(MAX_TABS);
    expect(capped.tabs[0]).toBe("session-3");
    expect(capped.active).toBe(`session-${MAX_TABS + 2}`);
  });

  it("activateTab：聚焦既有标签；聚焦分屏会话 → 两窗格对调", () => {
    const state = layout({ active: A, split: B });
    expect(activateTab(state, "ghost")).toBe(state);
    const swapped = activateTab(state, B);
    expect(swapped.active).toBe(B);
    expect(swapped.split).toBe(A);
  });

  it("closeTab：聚焦标签关闭 → 相邻标签；分屏标签关闭 → 关闭分屏", () => {
    const state = layout({ active: A, split: B });
    const closedActive = closeTab(state, A);
    expect(closedActive.tabs).toEqual([B]);
    expect(closedActive.active).toBe(B);
    expect(closedActive.split).toBeNull();
    expect(closeTab(state, "ghost")).toBe(state);

    const closedSplit = closeTab(layout({ active: A, split: B }), B);
    expect(closedSplit.tabs).toEqual([A]);
    expect(closedSplit.active).toBe(A);
    expect(closedSplit.split).toBeNull();

    const allClosed = closeTab(openTab(defaultLayout(), C), C);
    expect(allClosed.tabs).toEqual([]);
    expect(allClosed.active).toBeNull();
  });

  it("setSplit：非法目标（不在标签内/等于聚焦）→ 关闭分屏", () => {
    const state = layout({ active: A, split: null });
    expect(setSplit(state, B).split).toBe(B);
    expect(setSplit(state, A).split).toBeNull();
    expect(setSplit(state, C).split).toBeNull();
    expect(setSplit(state, null)).toBe(state);
  });

  it("clampLayout：裁剪不存在会话；active/split 归位；无变化保持引用", () => {
    const state = layout({ active: A, split: B });
    expect(clampLayout(state, [A, B, C])).toBe(state);

    const removedActive = clampLayout(state, [B]);
    expect(removedActive.tabs).toEqual([B]);
    expect(removedActive.active).toBe(B);
    expect(removedActive.split).toBeNull();

    const empty = clampLayout(state, []);
    expect(empty.tabs).toEqual([]);
    expect(empty.active).toBeNull();
    expect(empty.split).toBeNull();
  });
});
