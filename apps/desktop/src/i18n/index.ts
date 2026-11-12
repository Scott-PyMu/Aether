/**
 * M10-06 / ADR-036：轻量 i18n（无第三方依赖）。
 *
 * - 语言：`zh-CN` / `en-US`；`system` 由 OS locale 解析（`zh*` → zh-CN，其余 → en-US，
 *   无法检测 → zh-CN）。
 * - key：扁平命名空间 key（`app.*` / `session.*` / `error.*` / `a11y.*` 等）。
 * - 回退：当前语言 → zh-CN → 可见占位 `⟦key⟧`（不空白、不崩溃）。
 * - 插值：`{name}`；复数：调用方传入 `count` 时优先 `key.one` / `key.other`
 *   （`Intl.PluralRules`），缺失则回退基础 key。
 * - 日期/数字/相对时间统一走 `Intl.*`，按当前 locale。
 * - 运行期切换立即生效：`setActiveLocale` 更新全局状态并通知订阅者；
 *   组件通过 `useI18n()` / `useAppearance()` 订阅重渲染。
 */
import { tryAetherError } from "@aether/protocol";
import { useSyncExternalStore } from "react";

import enUS from "./locales/en-US.json";
import zhCN from "./locales/zh-CN.json";

export type Locale = "zh-CN" | "en-US";
export type LocaleSetting = "system" | Locale;

export const LOCALES: readonly Locale[] = ["zh-CN", "en-US"];
export const DEFAULT_LOCALE: Locale = "zh-CN";

type Dictionary = Record<string, string>;
const DICTIONARIES: Record<Locale, Dictionary> = {
  "zh-CN": zhCN as Dictionary,
  "en-US": enUS as Dictionary,
};

export type TranslateParams = Record<string, string | number | boolean | null | undefined>;

function normalizeLocaleTag(language: string | null | undefined): string {
  return (language ?? "").trim().toLowerCase();
}

/** OS locale 检测（ADR-036 D-036-1）：`zh*` → zh-CN，其余 → en-US，无法检测 → zh-CN。 */
export function detectSystemLocale(language?: string | null): Locale {
  const tag =
    language ??
    (typeof navigator !== "undefined" && typeof navigator.language === "string"
      ? navigator.language
      : "");
  if (tag.trim().length === 0) {
    return DEFAULT_LOCALE;
  }
  return normalizeLocaleTag(tag).startsWith("zh") ? "zh-CN" : "en-US";
}

/** 解析设置值；未知值按缺省 `system` 处理（核心侧已校验，前端兜底）。 */
export function resolveLocale(setting: LocaleSetting | string | null | undefined): Locale {
  if (setting === "zh-CN" || setting === "en-US") {
    return setting;
  }
  return detectSystemLocale();
}

let activeLocale: Locale = DEFAULT_LOCALE;
const listeners = new Set<() => void>();

export function getActiveLocale(): Locale {
  return activeLocale;
}

export function hasTranslation(key: string, locale: Locale = activeLocale): boolean {
  return Object.prototype.hasOwnProperty.call(DICTIONARIES[locale], key);
}

export function setActiveLocale(locale: Locale): void {
  if (activeLocale === locale) {
    return;
  }
  activeLocale = locale;
  if (typeof document !== "undefined") {
    document.documentElement.lang = locale;
  }
  for (const listener of listeners) {
    listener();
  }
}

export function subscribeI18n(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function pluralTemplate(key: string, count: number): string | undefined {
  const rule = new Intl.PluralRules(activeLocale).select(count);
  for (const candidate of [`${key}.${rule}`, `${key}.other`]) {
    const value = DICTIONARIES[activeLocale][candidate] ?? DICTIONARIES[DEFAULT_LOCALE][candidate];
    if (value !== undefined) {
      return value;
    }
  }
  return undefined;
}

/**
 * 翻译 key。缺失时回退 zh-CN，再回退可见占位（ADR-036 D-036-2）。
 * 注意：占位符使用 `⟦…⟧` 以便 E2E/单测断言「不空白」。
 */
export function t(key: string, params?: TranslateParams): string {
  let template = DICTIONARIES[activeLocale][key] ?? DICTIONARIES[DEFAULT_LOCALE][key];
  if (template === undefined) {
    return `⟦${key}⟧`;
  }
  if (params && typeof params.count === "number") {
    template = pluralTemplate(key, params.count) ?? template;
  }
  if (params) {
    template = template.replace(/\{(\w+)\}/g, (match, name: string) => {
      const value = params[name];
      return value === undefined || value === null ? match : String(value);
    });
  }
  return template;
}

/** 显式复数选择（与 `t(key, { count })` 等价；供不便传 count 的场景使用）。 */
export function plural(count: number, key: string, params?: TranslateParams): string {
  return t(key, { ...params, count });
}

/**
 * 已知 key → 本地化文案；未知值 → 原样返回（用于后端枚举/诊断数据的兜底，
 * 避免把未知 raw 值渲染成 `⟦…⟧`）。
 */
export function tKeyOrRaw(value: string): string {
  return hasTranslation(value) ? t(value) : value;
}

function intlLocale(): string {
  return activeLocale;
}

export function formatDate(value: Date | number, options?: Intl.DateTimeFormatOptions): string {
  return new Intl.DateTimeFormat(intlLocale(), options).format(value);
}

export function formatTime(value: Date | number): string {
  return formatDate(value, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function formatDateTime(value: Date | number): string {
  return formatDate(value, { dateStyle: "medium", timeStyle: "medium" });
}

export function formatNumber(value: number, options?: Intl.NumberFormatOptions): string {
  return new Intl.NumberFormat(intlLocale(), options).format(value);
}

export function formatRelativeTime(value: Date | number, base: Date | number = Date.now()): string {
  const deltaSeconds = Math.round((new Date(value).getTime() - new Date(base).getTime()) / 1000);
  const formatter = new Intl.RelativeTimeFormat(intlLocale(), { numeric: "auto" });
  const units: Array<[Intl.RelativeTimeFormatUnit, number]> = [
    ["year", 365 * 24 * 60 * 60],
    ["month", 30 * 24 * 60 * 60],
    ["day", 24 * 60 * 60],
    ["hour", 60 * 60],
    ["minute", 60],
    ["second", 1],
  ];
  for (const [unit, seconds] of units) {
    if (Math.abs(deltaSeconds) >= seconds || unit === "second") {
      return formatter.format(Math.round(deltaSeconds / seconds), unit);
    }
  }
  return formatter.format(0, "second");
}

export interface LocalizedError {
  title: string;
  message: string;
  /** 展示用的 code（business_code 优先，其次 aether_code）。 */
  code: string | null;
  /** 是否命中已登记错误码文案（false = 通用兜底）。 */
  known: boolean;
}

interface StructuredErrorLike {
  aether_code?: string | null;
  business_code?: string | null;
  message?: string | null;
}

function errorKeyCandidates(error: StructuredErrorLike): string[] {
  const candidates: string[] = [];
  if (error.business_code) {
    candidates.push(`error.business.${error.business_code}`);
  }
  if (error.aether_code) {
    candidates.push(`error.${error.aether_code}`);
  }
  return candidates;
}

/**
 * 错误本地化（ADR-036 D-036-4）：`business_code` → `aether_code` → 通用。
 * 后端 `message` 仅作未知 code 的兜底；已知 code 不直接展示后端文案。
 */
export function localizeError(error: unknown): LocalizedError {
  const raw =
    typeof error === "object" && error !== null ? (error as StructuredErrorLike) : null;
  const structured = (tryAetherError(error) as StructuredErrorLike | null) ?? raw;
  if (structured) {
    for (const base of errorKeyCandidates(structured)) {
      if (hasTranslation(`${base}.message`)) {
        return {
          title: t(`${base}.title`),
          message: t(`${base}.message`, { reason: structured.message ?? "" }),
          code: structured.business_code ?? structured.aether_code ?? null,
          known: true,
        };
      }
    }
  }
  const fallbackMessage =
    structured?.message ?? (error instanceof Error ? error.message : String(error));
  const code = structured?.business_code ?? structured?.aether_code ?? null;
  return {
    title: t("error.unknown.title"),
    message:
      fallbackMessage.trim().length > 0
        ? t("error.unknown.message", { code: code ?? "", reason: fallbackMessage })
        : t("error.unknown.message", { code: code ?? "", reason: "" }),
    code,
    known: false,
  };
}

/** 设置/读取 `<html lang>`（渲染器挂载时同步）。 */
export function syncDocumentLanguage(): void {
  if (typeof document !== "undefined") {
    document.documentElement.lang = activeLocale;
  }
}

/** React 订阅：语言切换时重渲染（不重载数据、不重启）。 */
export function useI18n(): { locale: Locale; t: typeof t } {
  const locale = useSyncExternalStore(subscribeI18n, getActiveLocale, getActiveLocale);
  return { locale, t };
}
