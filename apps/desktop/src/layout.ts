/**
 * 工作台多视图布局（M7-04；设计 §2.3「多视图」P1、UI-01 完整、§4#20、D8）。
 *
 * 口径：
 * - **落点** = settings 键 `ui.layout`（ADR-018 附录 A-6 登记：string（JSON 对象，
 *   ≤64KiB），缺省 `"{}"`；损坏回退默认布局）；本模块不新增表/迁移；
 * - **模型**：`tabs`（打开的会话视图，按标签顺序）+ `active`（聚焦会话 = 主窗格）
 *   + `split`（分屏第二窗格会话；`null` = 单窗格）；`split`/`active` 不得指向
 *   `tabs` 之外的会话，且二者不得相同；
 * - **损坏回退（M7-04 DoD3）**：非字符串值 / 非法 JSON / 非对象 / 版本不符 /
 *   字段类型非法 → 回退默认布局（空标签、无分屏）；引用不完整（`active`/`split`
 *   指向已不存在的标签）→ 静默归位（不视为损坏）；
 * - **首次运行**：设置行缺省 `{}`（无任何已知字段）→ 视为「无持久化布局」，
 *   由工作台延续 P0 行为自动打开首个会话；用户显式操作（打开/关闭/切换/分屏）
 *   后才写入持久化布局。
 */
import { UI_LAYOUT_KEY, type SettingsIpc } from "./settings";

/** 布局 schema 版本（未来不兼容变更时递增；未知版本按损坏回退默认）。 */
export const LAYOUT_VERSION = 1;

/** 打开标签上限（防御性上限；超过视为损坏回退默认）。 */
export const MAX_TABS = 32;

/** 工作台布局（持久化于 `ui.layout`）。 */
export interface WorkbenchLayout {
  version: number;
  /** 打开的会话视图（按标签顺序）。 */
  tabs: string[];
  /** 聚焦会话（主窗格；`null` = 无激活视图）。 */
  active: string | null;
  /** 分屏第二窗格会话（`null` = 单窗格）。 */
  split: string | null;
}

/** 默认布局（缺省 / 损坏回退）。 */
export function defaultLayout(): WorkbenchLayout {
  return { version: LAYOUT_VERSION, tabs: [], active: null, split: null };
}

/** 布局加载结果。 */
export interface LayoutLoadResult {
  layout: WorkbenchLayout;
  /** 存在持久化布局（含损坏 → 回退默认；此时不自动打开首个会话）。 */
  persisted: boolean;
  /** 数据损坏（非法 JSON / 形状非法 / 版本不符）→ 回退默认布局。 */
  corrupted: boolean;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * 解析 `ui.layout` 值（纯函数；损坏回退默认布局）。
 *
 * 输入为 `settings_get` 返回的 `value`（登记类型 = string）。
 */
export function parseLayoutValue(raw: unknown): LayoutLoadResult {
  if (raw === null || raw === undefined) {
    return { layout: defaultLayout(), persisted: false, corrupted: false };
  }
  if (typeof raw !== "string") {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  if (!isPlainObject(parsed)) {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  const hasKnownField =
    "version" in parsed || "tabs" in parsed || "active" in parsed || "split" in parsed;
  if (!hasKnownField) {
    // 缺省值 `"{}"`（设置行从未写入）：首次运行，无持久化布局。
    return { layout: defaultLayout(), persisted: false, corrupted: false };
  }
  if (parsed.version !== LAYOUT_VERSION) {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  const rawTabs = parsed.tabs;
  if (!Array.isArray(rawTabs) || rawTabs.some((tab) => typeof tab !== "string")) {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  const tabs: string[] = [];
  for (const tab of rawTabs) {
    if (!tabs.includes(tab)) {
      tabs.push(tab);
    }
  }
  if (tabs.length > MAX_TABS) {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  const rawActive = parsed.active ?? null;
  const rawSplit = parsed.split ?? null;
  if (
    (rawActive !== null && typeof rawActive !== "string") ||
    (rawSplit !== null && typeof rawSplit !== "string")
  ) {
    return { layout: defaultLayout(), persisted: true, corrupted: true };
  }
  // 引用归位：`active`/`split` 必须落在 `tabs` 内，且互不相同。
  const active = rawActive !== null && tabs.includes(rawActive) ? rawActive : (tabs[0] ?? null);
  const split =
    rawSplit !== null &&
    tabs.includes(rawSplit) &&
    rawSplit !== active
      ? rawSplit
      : null;
  return {
    layout: { version: LAYOUT_VERSION, tabs, active, split },
    persisted: true,
    corrupted: false,
  };
}

/** 读取并解析持久化布局；设置不可用（invoke 失败）→ 视为首次运行。 */
export async function loadLayout(ipc: SettingsIpc): Promise<LayoutLoadResult> {
  try {
    const result = await ipc.get(UI_LAYOUT_KEY);
    return parseLayoutValue(result.value);
  } catch {
    return { layout: defaultLayout(), persisted: false, corrupted: false };
  }
}

/**
 * 序列化并持久化布局（经 `settings_set`；失败静默返回 `false`——布局为
 * 非关键状态，不阻断工作台；下次显式操作重试）。
 */
export async function saveLayout(
  ipc: SettingsIpc,
  layout: WorkbenchLayout,
): Promise<boolean> {
  try {
    await ipc.set(UI_LAYOUT_KEY, JSON.stringify(layout));
    return true;
  } catch {
    return false;
  }
}

/** 打开标签（不存在则追加）并聚焦；已聚焦且已打开 → 原样返回。 */
export function openTab(layout: WorkbenchLayout, sessionId: string): WorkbenchLayout {
  const hasTab = layout.tabs.includes(sessionId);
  if (hasTab && layout.active === sessionId) {
    return layout;
  }
  let tabs = hasTab ? layout.tabs : [...layout.tabs, sessionId];
  if (tabs.length > MAX_TABS) {
    // 防御性上限：超出时丢弃最旧标签（保持持久化载荷可被下次解析接受）。
    tabs = tabs.slice(tabs.length - MAX_TABS);
  }
  let split = layout.split;
  if (sessionId === split) {
    // 聚焦分屏窗格会话 → 两窗格对调（同 activateTab）。
    split = layout.active;
  }
  if (split !== null && (!tabs.includes(split) || split === sessionId)) {
    split = null;
  }
  return { ...layout, tabs, active: sessionId, split };
}

/** 聚焦标签；若聚焦的是分屏窗格会话 → 两窗格对调（避免同会话双窗格）。 */
export function activateTab(layout: WorkbenchLayout, sessionId: string): WorkbenchLayout {
  if (!layout.tabs.includes(sessionId)) {
    return layout;
  }
  if (layout.active === sessionId) {
    return layout;
  }
  let split = layout.split;
  if (split === sessionId) {
    // 聚焦分屏窗格会话 → 两窗格对调（原聚焦会话进入第二窗格）。
    split = layout.active;
  }
  return { ...layout, active: sessionId, split };
}

/**
 * 关闭标签：移除视图；关闭的是聚焦会话 → 聚焦相邻标签（同位置后继，无则前驱）；
 * 关闭的是分屏会话 → 关闭分屏；无剩余标签 → 无激活视图（空态）。
 */
export function closeTab(layout: WorkbenchLayout, sessionId: string): WorkbenchLayout {
  const index = layout.tabs.indexOf(sessionId);
  if (index < 0) {
    return layout;
  }
  const tabs = layout.tabs.filter((tab) => tab !== sessionId);
  let active = layout.active;
  if (active === sessionId) {
    active = tabs[Math.min(index, tabs.length - 1)] ?? null;
  }
  let split = layout.split;
  if (split === sessionId || split === active) {
    split = null;
  }
  return { ...layout, tabs, active, split };
}

/** 设置分屏窗格（`null` 关闭分屏）；非法目标（不在标签内/等于聚焦）→ 关闭分屏。 */
export function setSplit(
  layout: WorkbenchLayout,
  sessionId: string | null,
): WorkbenchLayout {
  const next =
    sessionId !== null && layout.tabs.includes(sessionId) && sessionId !== layout.active
      ? sessionId
      : null;
  if (next === layout.split) {
    return layout;
  }
  return { ...layout, split: next };
}

/**
 * 清掉不在已知会话集合内的标签（会话被删除/列表刷新后）；`active`/`split`
 * 随之归位。无变化时原样返回（引用相等，避免无谓重渲染）。
 */
export function clampLayout(
  layout: WorkbenchLayout,
  sessionIds: readonly string[],
): WorkbenchLayout {
  const known = new Set(sessionIds);
  const tabs = layout.tabs.filter((tab) => known.has(tab));
  let active = layout.active !== null && known.has(layout.active) ? layout.active : null;
  if (active === null) {
    active = tabs[0] ?? null;
  }
  const split =
    layout.split !== null && known.has(layout.split) && layout.split !== active
      ? layout.split
      : null;
  const unchanged =
    tabs.length === layout.tabs.length &&
    tabs.every((tab, index) => tab === layout.tabs[index]) &&
    active === layout.active &&
    split === layout.split;
  if (unchanged) {
    return layout;
  }
  return { ...layout, tabs, active, split };
}
