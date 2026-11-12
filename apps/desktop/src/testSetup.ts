/**
 * M10-06：测试基线语言固定为 zh-CN（P0/P1 既有断言以中文文案为基线）。
 * 系统语言检测本身由 `i18n.test.tsx` 覆盖；外观切换用例显式设置语言。
 */
import { beforeEach } from "vitest";

import { resetAppearanceForTest } from "./appearance";
import { setActiveLocale } from "./i18n";

// P0/P1 既有断言以中文为基线；同时验证 `system` 在 zh-CN OS locale 下的解析。
Object.defineProperty(window.navigator, "language", {
  value: "zh-CN",
  configurable: true,
});

beforeEach(() => {
  resetAppearanceForTest();
  setActiveLocale("zh-CN");
});
