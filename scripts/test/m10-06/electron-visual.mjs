#!/usr/bin/env node
/**
 * `pnpm verify:m10-06:electron` — 真实 Electron 45 GUI 截图（启动/主界面/外观切换）。
 *
 * 仅本机/Windows GUI 环境运行；CI 无 GUI 时可跳过（verify:m10-06 仍校验 Chromium
 * headless 视觉基线）。使用 `scripts/test/m9-07/fixtures/fake-daemon.mjs`，不触碰
 * 用户数据目录。
 *
 * Windows build 26200 的 Electron 45 安装目录 ACL 问题见 `electron-acl-fix.mjs`
 * （electron/electron#51761 / #52255）；脚本自动使用临时修复副本。
 */
import { createRequire } from "node:module";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveFixedElectron } from "./electron-acl-fix.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..", "..", "..");
const desktopRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(desktopRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");
const fakeDaemon = path.join(repoRoot, "scripts", "test", "m9-07", "fixtures", "fake-daemon.mjs");
const outputDir = path.join(repoRoot, "docs", "M10-06-electron-visuals");
const generatedDir = path.join(repoRoot, "dist-out", "m10-06-electron-visuals");
const update = process.env.AETHER_VISUAL_UPDATE === "1";
const electronPath = process.env.AETHER_E2E_ELECTRON ?? resolveFixedElectron(repoRoot);

if (process.platform !== "win32" && !process.env.AETHER_E2E_ELECTRON) {
  console.error("本脚本面向 Windows GUI；其他平台请用 AETHER_E2E_ELECTRON 指定 Electron。");
  process.exit(1);
}
if (!existsSync(electronPath)) throw new Error(`Electron 不存在：${electronPath}`);
if (!existsSync(fakeDaemon)) throw new Error(`fake daemon 不存在：${fakeDaemon}`);

mkdirSync(generatedDir, { recursive: true });
const workspace = mkdtempSync(path.join(os.tmpdir(), "aether-m10-06-electron-"));
const dataDir = path.join(workspace, "data");
const userData = path.join(workspace, "user");
const registry = path.join(workspace, "registry");
for (const dir of [dataDir, userData, registry]) mkdirSync(dir, { recursive: true });

const env = {
  ...process.env,
  AETHER_E2E: "1",
  AETHER_USER_DATA_DIR: userData,
  AETHER_DATA_DIR: dataDir,
  AETHER_DATA_LOCATION_FILE: path.join(workspace, "data-location.json"),
  AETHER_RUNTIME_REGISTRY_DIR: registry,
  AETHERD_BIN: process.execPath,
  AETHERD_EXTRA_ARGS: JSON.stringify([fakeDaemon]),
  AETHER_FAKE_MODE: process.env.AETHER_FAKE_MODE ?? "healthy",
  AETHER_FAKE_STATE: path.join(workspace, "fake-state"),
};

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let lastError;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

async function snapshot(page, name) {
  const metrics = await page.evaluate(() => ({
    locale: document.documentElement.lang,
    theme: document.documentElement.dataset.theme,
    themeSource: document.documentElement.dataset.themeSource,
    viewport: { width: window.innerWidth, height: window.innerHeight },
    overflowX: document.documentElement.scrollWidth > window.innerWidth,
    title: document.title,
  }));
  const file = path.join(generatedDir, `${name}.png`);
  await page.screenshot({ path: file, fullPage: false });
  return {
    name,
    ...metrics,
    screenshotBytes: readFileSync(file).length,
  };
}

async function main() {
  const app = await _electron.launch({
    executablePath: electronPath,
    args: [repoRoot],
    cwd: repoRoot,
    env,
    timeout: 60_000,
  });
  try {
    const page = await app.firstWindow();
    await waitFor(
      async () => page.locator('[data-testid="workbench"]').count(),
      45_000,
      "主界面 workbench",
    );
    const timing = await app.evaluate(() => {
      const hook = globalThis.__aetherShell;
      return hook && typeof hook.snapshot === "function" ? hook.snapshot() : null;
    });
    if (timing) {
      console.log(
        `TIMING startedAt=${timing.startedAt} windowReadyAt=${timing.windowReadyAt} windowShownAt=${timing.windowShownAt} startupPageVisibleAt=${timing.startupPageVisibleAt} uiReadyAt=${timing.uiReadyAt} windowMs=${(timing.windowShownAt ?? 0) - timing.startedAt} uiMs=${(timing.uiReadyAt ?? 0) - timing.startedAt}`,
      );
    }
    if (process.env.AETHER_EXPECT_STARTUP_PAGE === "1") {
      if (timing && timing.startupPageVisibleAt === null) {
        throw new Error("慢启动应显示 S-00 启动页");
      }
    } else if (timing && timing.startupPageVisibleAt !== null) {
      throw new Error("正常启动不应显示 S-00 启动页");
    }
    if (timing && (timing.windowShownAt ?? 0) - timing.startedAt > 1_600) {
      throw new Error(`窗口显示超过预算：${(timing.windowShownAt ?? 0) - timing.startedAt}ms`);
    }
    const windowHandle = await app.browserWindow(page);
    await windowHandle.evaluate((window) => window.setContentSize(1280, 800));
    await sleep(300);

    const openSettings = async () => {
      if ((await page.locator('[data-testid="settings-page"]').count()) === 0) {
        await page.click('[data-testid="user-menu-toggle"]');
        await page.click('[data-testid="settings-open"]');
      }
      await waitFor(
        async () => page.locator('[data-testid="settings-page"]').count(),
        15_000,
        "设置页",
      );
    };
    const closeSettings = async () => {
      await page.click('[data-testid="settings-page"] [data-testid="overlay-back"]');
      await waitFor(
        async () => page.locator('[data-testid="workbench"]').count(),
        10_000,
        "返回工作台",
      );
    };

    // 显式选择 zh-CN + dark，避免依赖运行机器的系统主题。
    await openSettings();
    await page.selectOption('[data-testid="locale-select"]', "zh-CN");
    await page.selectOption('[data-testid="theme-select"]', "dark");
    await waitFor(
      async () => (await page.evaluate(() => document.documentElement.dataset.theme)) === "dark",
      10_000,
      "主题切 dark",
    );
    await closeSettings();

    const results = [];
    results.push(await snapshot(page, "zh-CN-dark-workbench"));

    await openSettings();
    results.push(await snapshot(page, "zh-CN-dark-settings"));

    await page.selectOption('[data-testid="theme-select"]', "light");
    await waitFor(
      async () => (await page.evaluate(() => document.documentElement.dataset.theme)) === "light",
      10_000,
      "主题切 light",
    );
    results.push(await snapshot(page, "zh-CN-light-settings"));

    await page.selectOption('[data-testid="locale-select"]', "en-US");
    await waitFor(
      async () => (await page.evaluate(() => document.documentElement.lang)) === "en-US",
      10_000,
      "语言切 en-US",
    );
    results.push(await snapshot(page, "en-US-light-settings"));

    await closeSettings();
    results.push(await snapshot(page, "en-US-light-workbench"));

    await openSettings();
    await page.selectOption('[data-testid="theme-select"]', "dark");
    await waitFor(
      async () => (await page.evaluate(() => document.documentElement.dataset.theme)) === "dark",
      10_000,
      "主题切 dark",
    );
    await closeSettings();
    results.push(await snapshot(page, "en-US-dark-workbench"));

    const failures = [];
    for (const result of results) {
      if (result.overflowX) failures.push(`${result.name}: 横向溢出`);
      if (result.viewport.width < 1200 || result.viewport.height < 700) {
        failures.push(`${result.name}: 视口 ${result.viewport.width}x${result.viewport.height} 过小`);
      }
      if (!result.screenshotBytes) failures.push(`${result.name}: 截图为空`);
    }
    const zhDark = results.find((item) => item.name === "zh-CN-dark-workbench");
    const enLight = results.find((item) => item.name === "en-US-light-workbench");
    if (!zhDark || !enLight) failures.push("缺少 zh-CN-dark / en-US-light 对照");
    else if (zhDark.theme === enLight.theme) failures.push("主题切换未反映到 data-theme");
    if (zhDark && zhDark.locale !== "zh-CN") failures.push(`中文语言异常：${zhDark.locale}`);
    if (enLight && enLight.locale !== "en-US") failures.push(`英文语言异常：${enLight.locale}`);

    writeFileSync(
      path.join(generatedDir, "metrics.json"),
      JSON.stringify({ generatedAt: new Date().toISOString(), electron: electronPath, results }, null, 2) + "\n",
    );
    mkdirSync(outputDir, { recursive: true });
    const baseline = path.join(outputDir, "metrics.json");
    if (update || !existsSync(baseline)) {
      for (const name of [...results.map((item) => `${item.name}.png`), "metrics.json"]) {
        copyFileSync(path.join(generatedDir, name), path.join(outputDir, name));
      }
      console.log(`BASELINE  recorded -> ${path.relative(repoRoot, outputDir)}`);
    } else {
      const expected = JSON.parse(readFileSync(baseline, "utf8"));
      for (const item of expected.results) {
        const actual = results.find((entry) => entry.name === item.name);
        if (!actual) {
          failures.push(`${item.name}: 缺失`);
          continue;
        }
        if (actual.locale !== item.locale || actual.theme !== item.theme) {
          failures.push(`${item.name}: locale/theme 回归`);
        }
        if (actual.viewport.width !== item.viewport.width || actual.viewport.height !== item.viewport.height) {
          failures.push(`${item.name}: 视口回归`);
        }
      }
    }

    if (failures.length > 0) {
      console.error("ELECTRON VISUAL FAILURES:");
      for (const failure of failures) console.error(`- ${failure}`);
      process.exitCode = 1;
      return;
    }
    console.log(`ELECTRON VISUAL OK  ${results.length} captures (${path.relative(repoRoot, generatedDir)})`);
  } finally {
    await app.close();
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
