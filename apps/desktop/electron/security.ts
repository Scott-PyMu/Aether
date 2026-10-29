/**
 * 安全基线（M9-07 DoD-2/DoD-5；ADR-029 §5.1）。
 *
 * 本模块只放**可单测的纯函数/常量**；Electron 接线在 `app.ts`：
 * - 生产 CSP：`connect-src 'none'`、`frame-src 'none'`（ADR-029 §5.1 原文）；
 * - 生产 renderer 经自定义 `aether://app` 协议加载（可设置响应头 CSP，
 *   避免 `file://` 无法下发的空窗）；dev 仅额外允许 Vite HMR 的 localhost；
 * - `will-navigate`/`setWindowOpenHandler`：只允许应用自身 URL，外链交系统浏览器，
 *   `file:`/`javascript:`/`data:`/`blob:` 一律拒绝；
 * - `webPreferences`：`nodeIntegration:false`、`contextIsolation:true`、
 *   `sandbox:true`、`webSecurity:true`、`allowRunningInsecureContent:false`、
 *   `webviewTag:false`。
 */
import path from "node:path";

import { EVENT_IPC_CHANNEL, RPC_IPC_CHANNEL, VERSION_IPC_CHANNEL } from "./ipcChannels";

export { EVENT_IPC_CHANNEL, RPC_IPC_CHANNEL, VERSION_IPC_CHANNEL };

/** 生产 renderer 自定义协议（标准、安全、可下发 CSP 响应头）。 */
export const APP_SCHEME = "aether";
/** 生产 renderer URL（`aether://app/index.html`）。 */
export const APP_URL = `${APP_SCHEME}://app/index.html`;
/** 自定义协议主机名。 */
export const APP_HOST = "app";
/** 主 UI preload 白名单（ADR-029 §5.1；不得增加）。 */
export const PRELOAD_API_KEYS = [
  "rpc",
  "onEvent",
  "offEvent",
  "pathForFile",
  "version",
] as const;

/** 生产 CSP（ADR-029 §5.1 逐字）。 */
export const PRODUCTION_CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data: blob:",
  "connect-src 'none'",
  "frame-src 'none'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

/** 开发 CSP：仅额外允许 Vite HMR 的 localhost（ADR-029 §5.1）。 */
export const DEV_CSP = [
  "default-src 'self'",
  "script-src 'self' 'unsafe-inline' 'unsafe-eval' http://127.0.0.1:5173",
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data: blob:",
  "connect-src 'self' ws://127.0.0.1:5173 http://127.0.0.1:5173",
  "frame-src 'none'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

/** 窗口尺寸（DoD-2）。 */
export const WINDOW_WIDTH = 1_280;
export const WINDOW_HEIGHT = 800;
export const WINDOW_MIN_WIDTH = 960;
export const WINDOW_MIN_HEIGHT = 640;
/** 主 UI 批处理窗口之外的 Vite 开发端口（固定 5173，strictPort）。 */
export const VITE_DEV_PORT = 5_173;
export const VITE_DEV_URL = `http://127.0.0.1:${VITE_DEV_PORT}`;

/** 导航决策。 */
export type NavigationDecision = "allow" | "external" | "deny";

function safeUrl(raw: string): URL | null {
  try {
    return new URL(raw);
  } catch {
    return null;
  }
}

/** 是否应用自身 URL（生产 `aether://app` 或开发 Vite origin）。 */
export function isInternalUrl(
  raw: string,
  options: { devServerUrl?: string } = {},
): boolean {
  const url = safeUrl(raw);
  if (url === null) {
    return false;
  }
  if (url.protocol === `${APP_SCHEME}:` && url.host === APP_HOST) {
    return true;
  }
  const devServerUrl = options.devServerUrl;
  if (devServerUrl !== undefined) {
    const dev = safeUrl(devServerUrl);
    if (dev !== null && url.origin === dev.origin) {
      return true;
    }
  }
  return false;
}

/** 顶层导航决策（`will-navigate`）。 */
export function decideNavigation(
  targetUrl: string,
  options: { devServerUrl?: string } = {},
): NavigationDecision {
  if (isInternalUrl(targetUrl, options)) {
    return "allow";
  }
  const url = safeUrl(targetUrl);
  if (url === null) {
    return "deny";
  }
  if (url.protocol === "http:" || url.protocol === "https:") {
    return "external";
  }
  // file:/javascript:/data:/blob:/about: 一律拒绝（ADR-029 §5.1）。
  return "deny";
}

/** 新窗口决策（`setWindowOpenHandler`）。 */
export function decideWindowOpen(targetUrl: string): NavigationDecision {
  const url = safeUrl(targetUrl);
  if (url === null) {
    return "deny";
  }
  if (url.protocol === "http:" || url.protocol === "https:") {
    return "external";
  }
  return "deny";
}

/** `BrowserWindow` 安全 webPreferences（DoD-2/DoD-5）。 */
export interface SecureWebPreferences {
  preload: string;
  nodeIntegration: false;
  nodeIntegrationInWorker: false;
  nodeIntegrationInSubFrames: false;
  contextIsolation: true;
  sandbox: true;
  webSecurity: true;
  allowRunningInsecureContent: false;
  webviewTag: false;
  spellcheck: false;
  experimentalFeatures: false;
}

export function secureWebPreferences(preload: string): SecureWebPreferences {
  return {
    preload,
    nodeIntegration: false,
    nodeIntegrationInWorker: false,
    nodeIntegrationInSubFrames: false,
    contextIsolation: true,
    sandbox: true,
    webSecurity: true,
    allowRunningInsecureContent: false,
    webviewTag: false,
    spellcheck: false,
    experimentalFeatures: false,
  };
}

/** 生产 renderer 文件目录（`apps/desktop/dist`）。 */
export function rendererDistDir(appPath: string): string {
  return path.join(appPath, "apps", "desktop", "dist");
}

/**
 * 把 `aether://app/...` 映射为 dist 内文件路径（防目录穿越）。
 *
 * - `aether://app/` / 无扩展名路径 → `index.html`（SPA 回退）；
 * - 只允许 dist 前缀内的文件；非法/越界返回 `null`。
 */
export function resolveAppAsset(distDir: string, requestUrl: string): string | null {
  const url = safeUrl(requestUrl);
  if (url === null || url.protocol !== `${APP_SCHEME}:` || url.host !== APP_HOST) {
    return null;
  }
  let pathname: string;
  try {
    pathname = decodeURIComponent(url.pathname);
  } catch {
    return null;
  }
  if (pathname.includes("\0")) {
    return null;
  }
  const relative = pathname.replace(/^\/+/, "");
  const candidate =
    relative.length === 0 || relative.endsWith("/")
      ? path.join(distDir, relative, "index.html")
      : path.join(distDir, relative);
  const normalizedDist = path.resolve(distDir);
  const normalized = path.resolve(candidate);
  if (
    normalized !== normalizedDist &&
    !normalized.startsWith(`${normalizedDist}${path.sep}`)
  ) {
    return null;
  }
  return normalized;
}

/** 扩展名 → Content-Type（只覆盖 renderer 构建产物）。 */
export function contentTypeFor(file: string): string {
  switch (path.extname(file).toLowerCase()) {
    case ".html":
      return "text/html; charset=utf-8";
    case ".js":
    case ".mjs":
      return "text/javascript; charset=utf-8";
    case ".css":
      return "text/css; charset=utf-8";
    case ".json":
      return "application/json; charset=utf-8";
    case ".svg":
      return "image/svg+xml";
    case ".png":
      return "image/png";
    case ".jpg":
    case ".jpeg":
      return "image/jpeg";
    case ".gif":
      return "image/gif";
    case ".webp":
      return "image/webp";
    case ".ico":
      return "image/x-icon";
    case ".woff":
      return "font/woff";
    case ".woff2":
      return "font/woff2";
    case ".ttf":
      return "font/ttf";
    case ".wasm":
      return "application/wasm";
    default:
      return "application/octet-stream";
  }
}

/** DevTools 是否可用（仅 debug 构建；生产禁用远程调试）。 */
export function devToolsEnabled(input: {
  isPackaged: boolean;
  env?: NodeJS.ProcessEnv;
}): boolean {
  const env = input.env ?? {};
  if (input.isPackaged) {
    return false;
  }
  return env.AETHER_DEVTOOLS !== "0";
}

/** DevTools 是否自动打开（默认不自动；显式 `AETHER_DEVTOOLS=1`）。 */
export function devToolsAutoOpen(env: NodeJS.ProcessEnv = {}): boolean {
  return env.AETHER_DEVTOOLS === "1";
}
