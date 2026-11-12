#!/usr/bin/env node
/**
 * `pnpm verify:m10-06:visual` — 截图/视觉回归矩阵（Chromium/Edge headless）。
 *
 * 覆盖 2 语言 × 2 解析后主题（light/dark；system 解析由单测覆盖）+ 960×640 最小布局。
 * 输出 PNG 截图与确定性 metrics（overflow/颜色 token/长文案宽度）。
 *
 * 首次运行或 `AETHER_VISUAL_UPDATE=1` 时记录基线（docs/M10-06-visuals/）；
 * 之后对比颜色 token / CSS token / overflow / 视口。字体渲染差异不参与逐像素比较。
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..", "..", "..");
const generatedDir = path.join(repoRoot, "dist-out", "m10-06-visuals");
const baselineDir = path.join(repoRoot, "docs", "M10-06-visuals");
const update = process.env.AETHER_VISUAL_UPDATE === "1";

const browser =
  process.env.AETHER_VISUAL_BROWSER ??
  [
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
  ].find((candidate) => existsSync(candidate));

if (!browser) {
  console.error("未找到 Chromium/Edge；可用 AETHER_VISUAL_BROWSER 指定路径");
  process.exit(1);
}

const dictionaries = {
  "zh-CN": JSON.parse(readFileSync(path.join(repoRoot, "apps/desktop/src/i18n/locales/zh-CN.json"), "utf8")),
  "en-US": JSON.parse(readFileSync(path.join(repoRoot, "apps/desktop/src/i18n/locales/en-US.json"), "utf8")),
};
const harness = readFileSync(path.join(here, "visual-harness.html"), "utf8").replace(
  '../../../apps/desktop/src/styles.css',
  pathToFileURL(path.join(repoRoot, "apps/desktop/src/styles.css")).href,
);
mkdirSync(generatedDir, { recursive: true });

function buildHarness(theme, locale, name) {
  const injected = `
    <script>
      applyHarness(${JSON.stringify(theme)}, ${JSON.stringify(locale)}, ${JSON.stringify(dictionaries[locale])});
      const harnessMetrics = window.harnessMetrics();
      document.body.setAttribute("data-harness-metrics", encodeURIComponent(JSON.stringify(harnessMetrics)));
    </script>
  `;
  const html = harness
    .replace('<html lang="zh-CN" data-theme="dark"', `<html lang="${locale}" data-theme="${theme}"`)
    .replace("</body>", `${injected}</body>`);
  const file = path.join(generatedDir, `${name}.html`);
  writeFileSync(file, html);
  return file;
}

function measure(file, size) {
  const dom = execFileSync(
    browser,
    [
      "--headless=new",
      "--disable-gpu",
      "--no-sandbox",
      "--hide-scrollbars",
      "--force-device-scale-factor=1",
      "--run-all-compositor-stages-before-draw",
      "--virtual-time-budget=2000",
      `--window-size=${size.width},${size.height}`,
      "--dump-dom",
      pathToFileURL(file).href,
    ],
    { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], timeout: 60000 },
  );
  const match = dom.match(/data-harness-metrics="([^"]+)"/);
  if (!match) throw new Error(`metrics missing: ${file}`);
  return JSON.parse(decodeURIComponent(match[1]));
}

function screenshot(file, size, output) {
  execFileSync(
    browser,
    [
      "--headless=new",
      "--disable-gpu",
      "--no-sandbox",
      "--hide-scrollbars",
      "--force-device-scale-factor=1",
      "--run-all-compositor-stages-before-draw",
      "--virtual-time-budget=2000",
      `--window-size=${size.width},${size.height}`,
      `--screenshot=${output}`,
      pathToFileURL(file).href,
    ],
    { encoding: "utf8", stdio: ["ignore", "ignore", "ignore"], timeout: 60000 },
  );
  if (!existsSync(output)) throw new Error(`screenshot missing: ${output}`);
}

const cases = [
  ["zh-CN", "light", { width: 1304, height: 892, contentWidth: 1280, contentHeight: 800 }],
  ["zh-CN", "dark", { width: 1304, height: 892, contentWidth: 1280, contentHeight: 800 }],
  ["en-US", "light", { width: 1304, height: 892, contentWidth: 1280, contentHeight: 800 }],
  ["en-US", "dark", { width: 1304, height: 892, contentWidth: 1280, contentHeight: 800 }],
  ["en-US", "light", { width: 984, height: 732, contentWidth: 960, contentHeight: 640 }],
].map(([locale, theme, size]) => {
  const name = `${locale}-${theme}-${size.contentWidth}x${size.contentHeight}`;
  return { name, locale, theme, size, expected: { width: size.contentWidth, height: size.contentHeight } };
});

const results = [];
for (const item of cases) {
  const file = buildHarness(item.theme, item.locale, item.name);
  const metrics = measure(file, item.size);
  const png = path.join(generatedDir, `${item.name}.png`);
  screenshot(file, item.size, png);
  results.push({ name: item.name, ...metrics, screenshotBytes: readFileSync(png).length });
}

writeFileSync(
  path.join(generatedDir, "metrics.json"),
  JSON.stringify({ generatedAt: new Date().toISOString(), browser, results }, null, 2) + "\n",
);

const failures = [];
for (const result of results) {
  if (result.overflowX) failures.push(`${result.name}: 横向溢出`);
  if (result.longTextOverflow) failures.push(`${result.name}: 长文案溢出`);
  if (result.viewport.width < 960 || result.viewport.height < 640) {
    failures.push(`${result.name}: 视口小于最小布局`);
  }
  if (!result.colors || !result.tokens) failures.push(`${result.name}: 颜色 metrics 缺失`);
}
const light = results.find((item) => item.name === "en-US-light-1280x800");
const dark = results.find((item) => item.name === "en-US-dark-1280x800");
if (!light || !dark) failures.push("缺少 light/dark 矩阵");
else {
  if (JSON.stringify(light.tokens) === JSON.stringify(dark.tokens)) failures.push("light/dark token 未变化");
  if (light.colors.bg === dark.colors.bg) failures.push("light/dark 背景色未变化");
}

mkdirSync(baselineDir, { recursive: true });
const baselineMetrics = path.join(baselineDir, "metrics.json");
if (update || !existsSync(baselineMetrics)) {
  for (const entry of readdirSync(generatedDir)) {
    if (entry.endsWith(".png") || entry === "metrics.json") {
      copyFileSync(path.join(generatedDir, entry), path.join(baselineDir, entry));
    }
  }
  console.log(`BASELINE  recorded -> ${path.relative(repoRoot, baselineDir)}`);
} else {
  const baseline = JSON.parse(readFileSync(baselineMetrics, "utf8"));
  for (const expected of baseline.results) {
    const actual = results.find((item) => item.name === expected.name);
    if (!actual) {
      failures.push(`${expected.name}: 缺失`);
      continue;
    }
    if (JSON.stringify(expected.colors) !== JSON.stringify(actual.colors)) {
      failures.push(`${expected.name}: 颜色 token 回归`);
    }
    if (JSON.stringify(expected.tokens) !== JSON.stringify(actual.tokens)) {
      failures.push(`${expected.name}: CSS token 回归`);
    }
    if (actual.locale !== expected.locale || actual.theme !== expected.theme) {
      failures.push(`${expected.name}: locale/theme 不匹配`);
    }
    if (actual.viewport.width !== expected.viewport.width || actual.viewport.height !== expected.viewport.height) {
      failures.push(`${expected.name}: 视口回归`);
    }
  }
}

if (failures.length > 0) {
  console.error("VISUAL FAILURES:");
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}
console.log(`VISUAL OK  ${results.length} captures (${path.relative(repoRoot, generatedDir)})`);
