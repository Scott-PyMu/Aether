/**
 * M10-06 / ADR-036：i18n 核心（语言检测/回退/插值/日期数字格式/错误码本地化）。
 */
import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import enUS from "./i18n/locales/en-US.json";
import zhCN from "./i18n/locales/zh-CN.json";
import {
  detectSystemLocale,
  formatDate,
  formatNumber,
  formatRelativeTime,
  getActiveLocale,
  hasTranslation,
  localizeError,
  resolveLocale,
  setActiveLocale,
  t,
  tKeyOrRaw,
} from "./i18n";

afterEach(() => {
  setActiveLocale("zh-CN");
});

describe("M10-06 i18n 字典", () => {
  it("zh-CN / en-US key 集合完全一致、无空值、无重复对象键", () => {
    const zhKeys = Object.keys(zhCN).sort();
    const enKeys = Object.keys(enUS).sort();
    expect(enKeys).toEqual(zhKeys);
    for (const key of zhKeys) {
      expect((zhCN as Record<string, string>)[key]?.trim().length).toBeGreaterThan(0);
      expect((enUS as Record<string, string>)[key]?.trim().length).toBeGreaterThan(0);
    }
  });

  it("32 个 aether_code 与 35 个 business_code 均有 title/message 本地化文案", () => {
    const codes = JSON.parse(
      readFileSync(path.resolve(process.cwd(), "../../packages/protocol/src/error_codes.json"), "utf8"),
    ) as { ipc: Array<{ code: string }>; business: Array<{ code: string }> };
    for (const { code } of codes.ipc) {
      expect(hasTranslation(`error.${code}.title`), code).toBe(true);
      expect(hasTranslation(`error.${code}.message`), code).toBe(true);
    }
    for (const { code } of codes.business) {
      expect(hasTranslation(`error.business.${code}.title`), code).toBe(true);
      expect(hasTranslation(`error.business.${code}.message`), code).toBe(true);
    }
  });
});

describe("语言解析与回退", () => {
  it("system 检测：zh* → zh-CN，其余 → en-US，空 → zh-CN", () => {
    expect(detectSystemLocale("zh-CN")).toBe("zh-CN");
    expect(detectSystemLocale("zh-TW")).toBe("zh-CN");
    expect(detectSystemLocale("en-GB")).toBe("en-US");
    expect(detectSystemLocale("fr-FR")).toBe("en-US");
    expect(detectSystemLocale("")).toBe("zh-CN");
    expect(resolveLocale("system")).toBe("zh-CN");
    expect(resolveLocale("en-US")).toBe("en-US");
    expect(resolveLocale("bogus")).toBe("zh-CN");
  });

  it("运行期切换更新 document.lang 与订阅者；缺失 key 回退可见占位（不空白/不崩溃）", () => {
    const seen: string[] = [];
    setActiveLocale("zh-CN");
    expect(document.documentElement.lang).toBe("zh-CN");
    setActiveLocale("en-US");
    expect(getActiveLocale()).toBe("en-US");
    expect(document.documentElement.lang).toBe("en-US");
    expect(t("app.common.about")).toBe("About");
    expect(t("missing.key.for.test")).toBe("⟦missing.key.for.test⟧");
    seen.push(t("app.common.about"));
    expect(seen).toEqual(["About"]);
  });

  it("未知 raw 值回退原文（后端枚举/诊断数据），已知 key 正常翻译", () => {
    setActiveLocale("en-US");
    expect(tKeyOrRaw("some_backend_value")).toBe("some_backend_value");
    expect(tKeyOrRaw("app.common.about")).toBe("About");
  });

  it("插值与复数入口不崩溃；缺失参数保留占位", () => {
    setActiveLocale("en-US");
    expect(t("settings.workspace_bound_new_sessions_inject", { p0: "C:\\ws" })).toContain("C:\\ws");
    expect(t("settings.workspace_bound_new_sessions_inject")).toContain("{p0}");
    expect(t("app.common.about", { count: 2 })).toBe("About");
  });
});

describe("日期/数字/相对时间", () => {
  it("zh-CN 与 en-US 输出随 locale 变化", () => {
    const ts = Date.UTC(2026, 10, 11, 12, 0, 0);
    setActiveLocale("zh-CN");
    const zhDate = formatDate(ts, { year: "numeric", month: "2-digit", day: "2-digit" });
    const zhNumber = formatNumber(1234567.89, { notation: 'compact' });
    const zhRelative = formatRelativeTime(ts - 2 * 24 * 60 * 60 * 1000, ts);
    setActiveLocale("en-US");
    const enDate = formatDate(ts, { year: "numeric", month: "2-digit", day: "2-digit" });
    const enNumber = formatNumber(1234567.89, { notation: 'compact' });
    const enRelative = formatRelativeTime(ts - 2 * 24 * 60 * 60 * 1000, ts);
    expect(zhDate).not.toBe(enDate);
    expect(zhNumber).not.toBe(enNumber);
    expect(zhRelative).not.toBe(enRelative);
  });
});

describe("错误码本地化（business_code → aether_code → 通用）", () => {
  it("business_code 优先", () => {
    setActiveLocale("en-US");
    const localized = localizeError({
      aether_code: "invalid_value",
      business_code: "session_busy",
      message: "后端原始中文",
    });
    expect(localized.code).toBe("session_busy");
    expect(localized.title).toBe("Session busy");
    expect(localized.message).not.toContain("后端原始中文");
  });

  it("仅 business_code 的对象也可本地化（预加载/旧信封兼容）", () => {
    setActiveLocale("en-US");
    const localized = localizeError({ business_code: "session_busy", message: "busy" });
    expect(localized.code).toBe("session_busy");
    expect(localized.title).toBe("Session busy");
    expect(localized.known).toBe(true);
  });

  it("无 business_code 时使用 aether_code", () => {
    setActiveLocale("zh-CN");
    const localized = localizeError({ aether_code: "provider_unreachable", message: "x" });
    expect(localized.code).toBe("provider_unreachable");
    expect(localized.title).toBe("供应商不可达");
  });

  it("未知 code 回退通用文案并保留 code/诊断 message", () => {
    setActiveLocale("zh-CN");
    const localized = localizeError({ aether_code: "made_up_code", message: "diagnostic detail" });
    expect(localized.code).toBe("made_up_code");
    expect(localized.title).toBe("操作失败");
    expect(localized.message).toContain("diagnostic detail");
  });
});
