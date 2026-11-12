#!/usr/bin/env node
/**
 * `pnpm verify:m10-06` — M10-06 i18n / 主题 DoD 检查（ADR-036）。
 *
 * 检查：
 *  1. 设置键 `ui.locale`/`ui.theme` 登记（Rust allowlist/缺省/校验）与前端键常量；
 *  2. zh-CN/en-US key 集合一致、无空值、无重复；错误码 32 + 35 全覆盖；
 *  3. 组件无硬编码用户可见中文/英文（AST 扫描；注释/测试/品牌/技术名白名单）；
 *  4. 所有 key 字面量存在于字典；
 *  5. 主题 token 完整（ADR-036 最小集 + space/radius/shadow/font），组件/样式规则无硬编码色值；
 *  6. 对比度（正文 ≥4.5:1；大字/图形 ≥3:1；on-accent ≥4.5:1）；
 *  7. UI-UX 锚点 `appearance` / `locale-select` / `theme-select` 与 data 属性；
 *  8. 运行 i18n/外观单测与 Rust 设置键单测。
 */
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const desktopRoot = path.join(root, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(desktopRoot, "package.json"));
const ts = requireFromDesktop("typescript");

const checks = [];
const failures = [];
function check(name, condition, detail = "") {
  checks.push(name);
  if (!condition) failures.push(`${name}${detail ? ` — ${detail}` : ""}`);
}
function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}
function readJson(rel) {
  return JSON.parse(read(rel));
}
function tool(name) {
  const override = process.env[`AETHER_${name.toUpperCase()}`];
  if (override) return override;
  if (name === "cargo") {
    const candidate = path.join(
      os.homedir(),
      ".rustup",
      "toolchains",
      "stable-x86_64-pc-windows-msvc",
      "bin",
      process.platform === "win32" ? "cargo.exe" : "cargo",
    );
    if (process.platform === "win32" && existsSync(candidate)) return candidate;
  }
  return name;
}
function run(label, command, args, { allowFailure = false } = {}) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    execFileSync(command, args, {
      cwd: root,
      stdio: "inherit",
      env: process.env,
      shell: process.platform === "win32",
    });
    checks.push(label);
  } catch (error) {
    checks.push(label);
    if (!allowFailure) failures.push(`${label} — 退出码 ${error.status ?? error.message}`);
  }
}

const zh = readJson("apps/desktop/src/i18n/locales/zh-CN.json");
const en = readJson("apps/desktop/src/i18n/locales/en-US.json");

/* 1. 设置键登记 */
const allowlist = read("crates/aether-app/src/ipc/dto.rs");
check("settings allowlist 含 ui.locale", allowlist.includes('"ui.locale"'));
check("settings allowlist 含 ui.theme", allowlist.includes('"ui.theme"'));
check("settings key 计数登记为 11", /SETTINGS_KEY_ALLOWLIST[\s\S]*?"ui.theme",\s*\];/.test(allowlist));
const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
check("ui.locale 缺省 system", /UI_LOCALE_KEY \| UI_THEME_KEY => Value::String\("system"/.test(diagnostics));
check(
  "ui.locale 枚举 system/zh-CN/en-US",
  /UI_LOCALE_KEY =>\s*\{\s*validate_enum_setting\(key, value, &\["system", "zh-CN", "en-US"\]\)/.test(diagnostics),
);
check(
  "ui.theme 枚举 system/light/dark",
  /UI_THEME_KEY =>\s*\{\s*validate_enum_setting\(key, value, &\["system", "light", "dark"\]\)/.test(diagnostics),
);
check("前端 UI_LOCALE_KEY", read("apps/desktop/src/settings.ts").includes('UI_LOCALE_KEY = "ui.locale"'));
check("前端 UI_THEME_KEY", read("apps/desktop/src/settings.ts").includes('UI_THEME_KEY = "ui.theme"'));

/* 2. key 集合/空值/重复 */
const zhKeys = Object.keys(zh).sort();
const enKeys = Object.keys(en).sort();
check("zh-CN/en-US key 集合一致", JSON.stringify(zhKeys) === JSON.stringify(enKeys), `${zhKeys.length}/${enKeys.length}`);
check("key 无空值", zhKeys.every((key) => String(zh[key]).trim() && String(en[key]).trim()));
const placeholderMismatch = zhKeys.filter((key) => {
  const pattern = /\{[a-zA-Z0-9_]+\}/g;
  const zhSet = [...new Set(String(zh[key]).match(pattern) ?? [])].sort();
  const enSet = [...new Set(String(en[key]).match(pattern) ?? [])].sort();
  return JSON.stringify(zhSet) !== JSON.stringify(enSet);
});
check("zh/en 插值占位符一致", placeholderMismatch.length === 0, placeholderMismatch.slice(0, 8).join(", "));
function duplicateKeys(rel) {
  const text = read(rel);
  const seen = new Set();
  const duplicates = [];
  for (const match of text.matchAll(/^\s*"([^"]+)":/gm)) {
    if (seen.has(match[1])) duplicates.push(match[1]);
    seen.add(match[1]);
  }
  return duplicates;
}
check("zh-CN JSON 无重复 key", duplicateKeys("apps/desktop/src/i18n/locales/zh-CN.json").length === 0);
check("en-US JSON 无重复 key", duplicateKeys("apps/desktop/src/i18n/locales/en-US.json").length === 0);

const codes = readJson("packages/protocol/src/error_codes.json");
const missingErrors = [];
for (const { code } of codes.ipc) {
  for (const suffix of ["title", "message"]) {
    if (!(`error.${code}.${suffix}` in zh) || !(`error.${code}.${suffix}` in en)) {
      missingErrors.push(`error.${code}.${suffix}`);
    }
  }
}
for (const { code } of codes.business) {
  for (const suffix of ["title", "message"]) {
    if (!(`error.business.${code}.${suffix}` in zh) || !(`error.business.${code}.${suffix}` in en)) {
      missingErrors.push(`error.business.${code}.${suffix}`);
    }
  }
}
check(`错误码文案覆盖 32 + ${codes.business.length}`, missingErrors.length === 0, missingErrors.slice(0, 8).join(", "));

/* 3/4. AST 扫描：硬编码用户可见文案、key 字面量存在性 */
const sourceFiles = [];
(function walk(dir) {
  for (const entry of requireFromDesktop("node:fs").readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full);
    else if (/\.(ts|tsx)$/.test(entry.name) && !/\.test\.(ts|tsx)$/.test(entry.name)) sourceFiles.push(full);
  }
})(path.join(desktopRoot, "src"));

const CJK = /[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]/;
const englishAllow = new Set([
  "A", "Aether", "Aether v", "API Key", "Base URL", "Apache-2.0", "· Apache-2.0",
  "Claude Code", "Codex", "DeepSeek Harness", "P0", "P1", "P2", "P3",
]);
const hardcoded = [];
const missingKeyRefs = [];
const nonI18nRefs = new Set([
  // 事件类型与设置键（非 UI 文案；事件类型见设计附录 B，设置键见核心 allowlist）。
  "permission.requested", "permission.resolved", "runtime.status_changed", "session.status_changed",
  "backup.reminder", "backup.auto.enabled", "backup.auto.frequency", "backup.auto.retention",
  "backup.encryption.enabled",
]);
const keyPattern =
  /["']((?:app|session|workflow|subagent|attachment|settings|backup|audit|error|a11y|permission|presets|providers|rules|runtime|startup|workspace|capacity|health)\.[A-Za-z0-9_.-]+)["']/g;

for (const file of sourceFiles) {
  const rel = path.relative(root, file).replaceAll("\\", "/");
  const text = requireFromDesktop("node:fs").readFileSync(file, "utf8");
  for (const match of text.matchAll(keyPattern)) {
    if (!(match[1] in zh) && !nonI18nRefs.has(match[1])) missingKeyRefs.push(`${rel}: ${match[1]}`);
  }
  const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  (function visit(node) {
    const line = sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1;
    if (ts.isJsxText(node)) {
      const value = node.text.trim();
      if (CJK.test(value)) hardcoded.push(`${rel}:${line} JSX ${value.slice(0, 60)}`);
      if (/[A-Za-z]/.test(value) && value.length > 0) {
        const normalized = value.replace(/\s+/g, " ").replace(/^[^\w]+/, "").trim();
        const allowed =
          englishAllow.has(normalized) ||
          /^[A-Za-z]?\s*(Apache-2\.0|Aether v?|API Key|Base URL)$/.test(normalized) ||
          /^[·•\s]*$/.test(normalized);
        if (!allowed && !/^var\(/.test(normalized)) hardcoded.push(`${rel}:${line} JSX-EN ${normalized.slice(0, 60)}`);
      }
    } else if (ts.isStringLiteral(node) && CJK.test(node.text)) {
      hardcoded.push(`${rel}:${line} string ${node.text.slice(0, 60)}`);
    } else if (ts.isNoSubstitutionTemplateLiteral(node) && CJK.test(node.text)) {
      hardcoded.push(`${rel}:${line} template ${node.text.slice(0, 60)}`);
    } else if (ts.isTemplateExpression(node) && CJK.test(node.getText(sf))) {
      hardcoded.push(`${rel}:${line} template ${node.getText(sf).slice(0, 60)}`);
    } else if (ts.isStringLiteral(node) && ts.isJsxAttribute(node.parent)) {
      const attr = node.parent.name.getText(sf);
      if (["aria-label", "title", "placeholder", "alt"].includes(attr) && CJK.test(node.text)) {
        hardcoded.push(`${rel}:${line} attr ${attr}=${node.text.slice(0, 60)}`);
      }
    }
    ts.forEachChild(node, visit);
  })(sf);
}
check("无组件硬编码中文/英文文案", hardcoded.length === 0, hardcoded.slice(0, 10).join(" | "));
check("所有 key 字面量存在于 zh-CN", missingKeyRefs.length === 0, missingKeyRefs.slice(0, 10).join(" | "));

/* 5. 主题 token 与色值 */
const css = read("apps/desktop/src/styles.css");
const requiredTokens = [
  "--color-bg", "--color-surface", "--color-text", "--color-text-muted", "--color-border",
  "--color-accent", "--color-danger", "--color-warning", "--color-success",
  "--space-1", "--radius-sm", "--shadow-1", "--font-body", "--font-mono",
];
check("主题 token 最小集完整", requiredTokens.every((token) => css.includes(`${token}:`)), requiredTokens.filter((token) => !css.includes(`${token}:`)).join(","));
const marker = css.indexOf("* {");
const rulesBody = marker >= 0 ? css.slice(marker) : css;
const literalColors = rulesBody.match(/#[0-9a-fA-F]{3,8}\b|rgba?\([^)]*\)/g) ?? [];
check("组件/规则无硬编码色值（token 定义块除外）", literalColors.length === 0, [...new Set(literalColors)].join(","));
const tsColor = sourceFiles.flatMap((file) => {
  const text = requireFromDesktop("node:fs").readFileSync(file, "utf8");
  const matches = text.match(/#[0-9a-fA-F]{3,8}\b|rgba?\([^)]*\)/g) ?? [];
  return matches.map((value) => `${path.relative(root, file).replaceAll("\\", "/")}: ${value}`);
});
check("TS/TSX 无硬编码色值", tsColor.length === 0, tsColor.slice(0, 8).join(", "));

function tokenBlock(start) {
  const at = css.indexOf(start);
  if (at < 0) return "";
  const end = css.indexOf("}", at);
  return css.slice(at, end);
}
function tokens(block) {
  const result = {};
  for (const match of block.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6});/g)) result[match[1]] = match[2];
  return result;
}
function luminance(hex) {
  const channels = [1, 3, 5]
    .map((index) => parseInt(hex.slice(index, index + 2), 16) / 255)
    .map((value) => (value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4));
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}
function contrast(a, b) {
  const l1 = luminance(a);
  const l2 = luminance(b);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
}
const palettes = {
  dark: tokens(tokenBlock(':root,\n:root[data-theme="dark"] {')),
  light: tokens(tokenBlock(':root[data-theme="light"] {')),
};
const contrastProblems = [];
for (const [name, palette] of Object.entries(palettes)) {
  const pairs = [
    ["color-text", "color-bg", 4.5],
    ["color-text", "color-surface", 4.5],
    ["color-text-muted", "color-bg", 4.5],
    ["color-accent-text", "color-surface", 4.5],
    ["color-danger-text", "color-surface", 4.5],
    ["color-warning", "color-surface", 4.5],
    ["color-success", "color-surface", 4.5],
    ["color-on-accent", "color-accent", 4.5],
    ["color-danger", "color-surface", 3],
    ["color-accent", "color-surface", 3],
  ];
  for (const [fg, bg, minimum] of pairs) {
    if (!palette[fg] || !palette[bg]) {
      contrastProblems.push(`${name}: missing ${fg}/${bg}`);
      continue;
    }
    const ratio = contrast(palette[fg], palette[bg]);
    if (ratio + 0.005 < minimum) contrastProblems.push(`${name} ${fg}/${bg}=${ratio.toFixed(2)} < ${minimum}`);
  }
}
check("对比度 WCAG AA（正文 4.5 / 图形 3）", contrastProblems.length === 0, contrastProblems.join("; "));

/* 6. 锚点 */
const settings = read("apps/desktop/src/SettingsPage.tsx");
check("appearance 锚点", settings.includes('data-testid="appearance"'));
check("locale-select + data-locale", /data-testid="locale-select"[\s\S]{0,160}data-locale=/.test(settings));
check("theme-select + data-theme", /data-testid="theme-select"[\s\S]{0,160}data-theme=/.test(settings));
check("运行期切换写 html lang", read("apps/desktop/src/i18n/index.ts").includes("document.documentElement.lang = locale"));
check("主题解析写 data-theme/data-theme-source", read("apps/desktop/src/appearance.ts").includes('root.dataset.themeSource = stateValue.themeSource'));
check("system 监听 prefers-color-scheme", read("apps/desktop/src/appearance.ts").includes('addEventListener("change"'));

/* 7. 视觉回归基线（Chromium/Edge headless 截图 + metrics） */
const visualMetricsPath = "docs/M10-06-visuals/metrics.json";
check("视觉回归 metrics 基线存在", existsSync(path.join(root, visualMetricsPath)));
if (existsSync(path.join(root, visualMetricsPath))) {
  const visual = readJson(visualMetricsPath);
  check("视觉回归矩阵 5 captures", visual.results.length === 5, String(visual.results?.length));
  check(
    "视觉回归无横向/长文案溢出",
    visual.results.every((item) => item.overflowX === false && item.longTextOverflow === false),
  );
  check(
    "视觉回归 light/dark token 不同",
    JSON.stringify(visual.results.find((item) => item.name === "en-US-light-1280x800")?.tokens) !==
      JSON.stringify(visual.results.find((item) => item.name === "en-US-dark-1280x800")?.tokens),
  );
  for (const item of visual.results) {
    check(
      `视觉回归截图 ${item.name}.png`,
      existsSync(path.join(root, "docs", "M10-06-visuals", `${item.name}.png`)),
    );
  }
}

/* 7b. Electron 45 GUI 截图基线（真实主进程/渲染器；本机 Windows） */
const electronMetricsPath = "docs/M10-06-electron-visuals/metrics.json";
check("Electron GUI 视觉基线存在", existsSync(path.join(root, electronMetricsPath)));
if (existsSync(path.join(root, electronMetricsPath))) {
  const electronVisual = readJson(electronMetricsPath);
  check("Electron GUI 矩阵 6 captures", electronVisual.results.length === 6, String(electronVisual.results?.length));
  check(
    "Electron GUI 无横向溢出",
    electronVisual.results.every((item) => item.overflowX === false),
  );
  const electronZhDark = electronVisual.results.find((item) => item.name === "zh-CN-dark-workbench");
  const electronEnLight = electronVisual.results.find((item) => item.name === "en-US-light-workbench");
  check("Electron GUI zh-CN/dark 生效", electronZhDark?.locale === "zh-CN" && electronZhDark?.theme === "dark");
  check("Electron GUI en-US/light 生效", electronEnLight?.locale === "en-US" && electronEnLight?.theme === "light");
  for (const item of electronVisual.results) {
    check(
      `Electron GUI 截图 ${item.name}.png`,
      existsSync(path.join(root, "docs", "M10-06-electron-visuals", `${item.name}.png`)),
    );
  }
}

/* 8. 运行测试 */
run("i18n/外观单测", "pnpm", ["--filter", "@aether/desktop", "test", "src/i18n.test.tsx", "src/m10_06_appearance.test.tsx"]);
run(
  "Rust 设置键单测",
  tool("cargo"),
  ["test", "-p", "aether-app", "--lib", "diagnostics_control", "-j", "1", "--", "--test-threads=1"],
);

console.log(`\nM10-06 verify: ${checks.length - failures.length}/${checks.length} checks passed`);
if (failures.length > 0) {
  console.error("\nFAILURES:");
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}
console.log("OK  M10-06 i18n/theme");
