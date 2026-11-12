/**
 * M10-06 / ADR-036：外观（语言 + 主题）运行期状态。
 *
 * - 设置键：`ui.locale`（system/zh-CN/en-US）、`ui.theme`（system/light/dark）；
 * - 运行期切换立即生效，不重启、不重载数据；
 * - 主题解析后写 `<html data-theme="light|dark" data-theme-source="system|explicit">`；
 * - system 模式监听 `prefers-color-scheme`；
 * - 无第三方依赖；持久化复用 `settings_get/settings_set`。
 */
import { useSyncExternalStore } from "react";

import {
  detectSystemLocale,
  resolveLocale,
  setActiveLocale,
  syncDocumentLanguage,
  type Locale,
  type LocaleSetting,
} from "./i18n";
import {
  settingsIpc,
  UI_LOCALE_KEY,
  UI_THEME_KEY,
  type SettingsIpc,
} from "./settings";

export type ThemeSetting = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";
export type ThemeSource = "system" | "explicit";

export const THEME_SETTINGS: readonly ThemeSetting[] = ["system", "light", "dark"];

export interface AppearanceState {
  localeSetting: LocaleSetting;
  themeSetting: ThemeSetting;
  locale: Locale;
  theme: ResolvedTheme;
  themeSource: ThemeSource;
}

function matchMediaQuery(): MediaQueryList | null {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
    return null;
  }
  return window.matchMedia("(prefers-color-scheme: light)");
}

/** 系统主题检测：`prefers-color-scheme: light` → light，否则 dark（P0 深色基线）。 */
export function detectSystemTheme(
  query: MediaQueryList | null = matchMediaQuery(),
): ResolvedTheme {
  return query?.matches ? "light" : "dark";
}

export function resolveTheme(
  setting: ThemeSetting | string | null | undefined,
  systemTheme: ResolvedTheme = detectSystemTheme(),
): ResolvedTheme {
  if (setting === "light" || setting === "dark") {
    return setting;
  }
  return systemTheme;
}

export function normalizeLocaleSetting(value: unknown): LocaleSetting {
  return value === "zh-CN" || value === "en-US" ? value : "system";
}

export function normalizeThemeSetting(value: unknown): ThemeSetting {
  return value === "light" || value === "dark" ? value : "system";
}

function initialState(): AppearanceState {
  const localeSetting: LocaleSetting = "system";
  const themeSetting: ThemeSetting = "system";
  return {
    localeSetting,
    themeSetting,
    locale: detectSystemLocale(),
    theme: detectSystemTheme(),
    themeSource: "system",
  };
}

let state: AppearanceState = initialState();
const listeners = new Set<() => void>();
let media: MediaQueryList | null = null;
let mediaBound = false;
let initPromise: Promise<AppearanceState> | null = null;

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

export function getAppearanceState(): AppearanceState {
  return state;
}

export function subscribeAppearance(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function applyDocument(stateValue: AppearanceState): void {
  if (typeof document === "undefined") {
    return;
  }
  const root = document.documentElement;
  root.dataset.theme = stateValue.theme;
  root.dataset.themeSource = stateValue.themeSource;
  root.style.colorScheme = stateValue.theme;
  setActiveLocale(stateValue.locale);
  syncDocumentLanguage();
}

function commit(next: AppearanceState): void {
  state = next;
  applyDocument(state);
  emit();
}

function bindSystemTheme(): void {
  if (mediaBound) {
    return;
  }
  mediaBound = true;
  media = matchMediaQuery();
  if (media && typeof media.addEventListener === "function") {
    media.addEventListener("change", () => {
      if (state.themeSetting === "system") {
        commit({ ...state, theme: detectSystemTheme(media), themeSource: "system" });
      }
    });
  }
}

/** 解析 settings 值并应用（缺省/损坏值按 `system`）。 */
export function applyAppearanceSettings(
  localeValue: unknown,
  themeValue: unknown,
  systemTheme: ResolvedTheme = detectSystemTheme(),
): AppearanceState {
  const localeSetting = normalizeLocaleSetting(localeValue);
  const themeSetting = normalizeThemeSetting(themeValue);
  const next: AppearanceState = {
    localeSetting,
    themeSetting,
    locale: resolveLocale(localeSetting),
    theme: resolveTheme(themeSetting, systemTheme),
    themeSource: themeSetting === "system" ? "system" : "explicit",
  };
  bindSystemTheme();
  commit(next);
  return next;
}

/**
 * 启动时读取已持久化的外观设置。失败时保留默认（system/system），不阻塞主界面。
 */
export async function initAppearance(ipc: SettingsIpc = settingsIpc): Promise<AppearanceState> {
  bindSystemTheme();
  if (initPromise) {
    return initPromise;
  }
  initPromise = (async () => {
    let localeValue: unknown = "system";
    let themeValue: unknown = "system";
    try {
      const [localeResult, themeResult] = await Promise.all([
        ipc.get(UI_LOCALE_KEY),
        ipc.get(UI_THEME_KEY),
      ]);
      localeValue = localeResult.value;
      themeValue = themeResult.value;
    } catch {
      // 启动门之外的只读读取失败：保持 system 缺省（不阻塞主界面）。
    }
    return applyAppearanceSettings(localeValue, themeValue);
  })();
  return initPromise;
}

/** 仅用于测试：重置 store（不触碰持久化）。 */
export function resetAppearanceForTest(): void {
  initPromise = null;
  state = initialState();
  if (typeof document !== "undefined") {
    delete document.documentElement.dataset.theme;
    delete document.documentElement.dataset.themeSource;
    document.documentElement.removeAttribute("style");
  }
  setActiveLocale(detectSystemLocale());
  emit();
}

export async function setLocaleSetting(
  setting: LocaleSetting,
  ipc: SettingsIpc = settingsIpc,
): Promise<AppearanceState> {
  const next: AppearanceState = {
    ...state,
    localeSetting: setting,
    locale: resolveLocale(setting),
  };
  commit(next);
  await ipc.set(UI_LOCALE_KEY, setting);
  return state;
}

export async function setThemeSetting(
  setting: ThemeSetting,
  ipc: SettingsIpc = settingsIpc,
): Promise<AppearanceState> {
  const next: AppearanceState = {
    ...state,
    themeSetting: setting,
    theme: resolveTheme(setting),
    themeSource: setting === "system" ? "system" : "explicit",
  };
  commit(next);
  await ipc.set(UI_THEME_KEY, setting);
  return state;
}

/** React 订阅：语言/主题切换时重渲染（不重新加载数据）。 */
export function useAppearance(): AppearanceState & {
  locale: Locale;
  setLocale: (setting: LocaleSetting, ipc?: SettingsIpc) => Promise<AppearanceState>;
  setTheme: (setting: ThemeSetting, ipc?: SettingsIpc) => Promise<AppearanceState>;
} {
  const snapshot = useSyncExternalStore(subscribeAppearance, getAppearanceState, getAppearanceState);
  return {
    ...snapshot,
    setLocale: setLocaleSetting,
    setTheme: setThemeSetting,
  };
}

// 首帧即应用缺省外观（system/system），避免浅色系统下的深色闪烁。
if (typeof document !== "undefined") {
  applyDocument(state);
}
