/**
 * Electron 壳安全基线单测（M9-07 DoD-2/DoD-5；ADR-029 §5.1）。
 */
import path from "node:path";

import { describe, expect, it } from "vitest";

import {
  APP_URL,
  DEV_CSP,
  PRELOAD_API_KEYS,
  PRODUCTION_CSP,
  contentTypeFor,
  decideNavigation,
  decideWindowOpen,
  devToolsAutoOpen,
  devToolsEnabled,
  isInternalUrl,
  resolveAppAsset,
  secureWebPreferences,
} from "./security";

describe("security baseline", () => {
  it("生产 CSP：connect-src/frame-src 均为 none（DoD-5）", () => {
    expect(PRODUCTION_CSP).toContain("connect-src 'none'");
    expect(PRODUCTION_CSP).toContain("frame-src 'none'");
    expect(PRODUCTION_CSP).toContain("default-src 'self'");
    expect(PRODUCTION_CSP).toContain("object-src 'none'");
    expect(PRODUCTION_CSP).toContain("base-uri 'none'");
    expect(PRODUCTION_CSP).toContain("form-action 'none'");
    expect(DEV_CSP).toContain("ws://127.0.0.1:5173");
    expect(DEV_CSP).toContain("connect-src 'self' ws://127.0.0.1:5173 http://127.0.0.1:5173");
  });

  it("preload 白名单固定为 5 项（DoD-2）", () => {
    expect([...PRELOAD_API_KEYS]).toEqual([
      "rpc",
      "onEvent",
      "offEvent",
      "pathForFile",
      "version",
    ]);
  });

  it("webPreferences：nodeIntegration/contextIsolation/sandbox/webviewTag", () => {
    const prefs = secureWebPreferences("C:\\app\\preload.cjs");
    expect(prefs.nodeIntegration).toBe(false);
    expect(prefs.contextIsolation).toBe(true);
    expect(prefs.sandbox).toBe(true);
    expect(prefs.webSecurity).toBe(true);
    expect(prefs.allowRunningInsecureContent).toBe(false);
    expect(prefs.webviewTag).toBe(false);
    expect(prefs.nodeIntegrationInSubFrames).toBe(false);
    expect(prefs.preload).toBe("C:\\app\\preload.cjs");
  });

  it("应用自身 URL：生产 aether://app 与 dev Vite origin 允许", () => {
    expect(isInternalUrl(APP_URL)).toBe(true);
    expect(isInternalUrl("aether://app/assets/index.js")).toBe(true);
    expect(
      isInternalUrl("http://127.0.0.1:5173/src/main.tsx", {
        devServerUrl: "http://127.0.0.1:5173",
      }),
    ).toBe(true);
    expect(isInternalUrl("https://example.com")).toBe(false);
    expect(isInternalUrl("file:///C:/Aether/index.html")).toBe(false);
  });

  it("顶层导航：外链 external，file/javascript/data/blob 拒绝", () => {
    expect(decideNavigation(APP_URL)).toBe("allow");
    expect(decideNavigation("https://example.com/docs")).toBe("external");
    expect(decideNavigation("http://example.com")).toBe("external");
    expect(decideNavigation("file:///C:/Windows/System32/drivers/etc/hosts")).toBe(
      "deny",
    );
    expect(decideNavigation("javascript:alert(1)")).toBe("deny");
    expect(decideNavigation("data:text/html,<h1>x</h1>")).toBe("deny");
    expect(decideNavigation("blob:https://example.com/x")).toBe("deny");
    expect(decideNavigation("not a url")).toBe("deny");
  });

  it("新窗口：http/https external，其余拒绝", () => {
    expect(decideWindowOpen("https://example.com")).toBe("external");
    expect(decideWindowOpen("http://example.com")).toBe("external");
    expect(decideWindowOpen("file:///C:/Windows/win.ini")).toBe("deny");
    expect(decideWindowOpen(APP_URL)).toBe("deny");
  });

  it("aether://app 资源解析：防目录穿越 + SPA 回退", () => {
    const dist = path.resolve("C:/Aether/apps/desktop/dist");
    expect(resolveAppAsset(dist, "aether://app/index.html")).toBe(
      path.join(dist, "index.html"),
    );
    expect(resolveAppAsset(dist, "aether://app/assets/index.js")).toBe(
      path.join(dist, "assets", "index.js"),
    );
    expect(resolveAppAsset(dist, "aether://app/")).toBe(
      path.join(dist, "index.html"),
    );
    expect(resolveAppAsset(dist, "aether://app/assets/..%2f..%2fsecret.txt")).toBeNull();
    expect(resolveAppAsset(dist, "aether://other/index.html")).toBeNull();
    expect(resolveAppAsset(dist, "https://example.com/index.html")).toBeNull();
  });

  it("Content-Type 覆盖 renderer 构建产物", () => {
    expect(contentTypeFor("index.html")).toContain("text/html");
    expect(contentTypeFor("index.js")).toContain("text/javascript");
    expect(contentTypeFor("index.css")).toContain("text/css");
    expect(contentTypeFor("logo.svg")).toBe("image/svg+xml");
    expect(contentTypeFor("font.woff2")).toBe("font/woff2");
    expect(contentTypeFor("blob.bin")).toBe("application/octet-stream");
  });

  it("DevTools 仅 debug：打包后关闭，默认不自动打开", () => {
    expect(devToolsEnabled({ isPackaged: true })).toBe(false);
    expect(devToolsEnabled({ isPackaged: false, env: {} })).toBe(true);
    expect(devToolsEnabled({ isPackaged: false, env: { AETHER_DEVTOOLS: "0" } })).toBe(
      false,
    );
    expect(devToolsAutoOpen({})).toBe(false);
    expect(devToolsAutoOpen({ AETHER_DEVTOOLS: "1" })).toBe(true);
  });
});
