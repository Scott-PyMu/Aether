/**
 * `WebContentsView` 浏览器 PoC 的安全策略（M9-10 DoD-4；ADR-029 D-029-7/§5.2）。
 *
 * M9 只做 PoC + 安全基线：完整 UI-06 交互（标签/分屏/下载 UI/权限提示）属 P4。
 * 本模块只含**可单测的纯函数/状态机**，Electron 接线在 `browserHost.ts`：
 * - 独立 `session.fromPartition('persist:aether-browser:<id>')`（不与主 UI 共享）；
 * - 无 preload / 无 Node / 无 `window.aether`；
 * - 仅允许 `http:`/`https:` 顶层导航；`file:`/`javascript:`/`data:`/`blob:` 拒绝；
 * - `window.open` 交系统浏览器（不创建新窗口）；
 * - 权限默认拒绝（不设 allowlist）；
 * - 下载必须用户确认后才落盘；文件名清洗 + 不自动打开/执行。
 */
import path from "node:path";

/** 浏览器视图 partition 前缀（每个视图一个独立存储）。 */
export const BROWSER_PARTITION_PREFIX = "persist:aether-browser:";
/** 同一视图最多排队的未确认下载数。 */
export const MAX_PENDING_DOWNLOADS = 8;
/** 下载文件名最大字符数（含扩展名）。 */
export const MAX_DOWNLOAD_FILE_NAME = 128;

const PARTITION_ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
const WINDOWS_RESERVED = /^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\..*)?$/i;

/**
 * 浏览器视图 contents 标记：`web-contents-created` 在 `new WebContentsView()`
 * 内同步触发，无法先于事件注册；main 的统一加固延迟到 `setImmediate`，由
 * `BrowserHost` 在创建后同步标记，避免把主 UI 的 `aether://app` 白名单误套到
 * 浏览器视图（Electron 45 的 `getType()` 对 WebContentsView 仍返回 `window`）。
 */
const browserContentsRegistry = new WeakSet<object>();

export function markBrowserContents(contents: object): void {
  browserContentsRegistry.add(contents);
}

export function isBrowserContents(contents: object): boolean {
  return browserContentsRegistry.has(contents);
}

/** 由视图 id 生成独立 partition（拒绝非法 id，避免拼出共享/越权分区）。 */
export function browserPartition(id: string): string {
  if (!PARTITION_ID.test(id)) {
    throw new TypeError(
      "浏览器视图 id 只允许 1–64 位字母/数字/下划线/点/连字符，且以字母或数字开头",
    );
  }
  return `${BROWSER_PARTITION_PREFIX}${id}`;
}

/** 浏览器视图 webPreferences：无 preload、无 Node、独立沙箱。 */
export interface BrowserWebPreferences {
  sandbox: true;
  contextIsolation: true;
  nodeIntegration: false;
  nodeIntegrationInWorker: false;
  nodeIntegrationInSubFrames: false;
  webSecurity: true;
  allowRunningInsecureContent: false;
  webviewTag: false;
  spellcheck: false;
  experimentalFeatures: false;
}

export function browserWebPreferences(): BrowserWebPreferences {
  return {
    sandbox: true,
    contextIsolation: true,
    nodeIntegration: false,
    nodeIntegrationInWorker: false,
    nodeIntegrationInSubFrames: false,
    webSecurity: true,
    allowRunningInsecureContent: false,
    webviewTag: false,
    spellcheck: false,
    experimentalFeatures: false,
  };
}

/** 浏览器视图导航决策：只允许 http/https。 */
export function decideBrowserNavigation(raw: string): "allow" | "deny" {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return "deny";
  }
  return url.protocol === "http:" || url.protocol === "https:" ? "allow" : "deny";
}

/** 浏览器视图 `window.open` 决策：http/https 交系统浏览器，其余拒绝。 */
export function decideBrowserWindowOpen(raw: string): "external" | "deny" {
  return decideBrowserNavigation(raw) === "allow" ? "external" : "deny";
}

/** 权限决策：默认拒绝；M9 PoC 不开放任何能力（P4 再按项 allowlist）。 */
export function decideBrowserPermission(_permission: string): false {
  return false;
}

/**
 * 下载文件名清洗（ADR-029 §5.2）：
 * - 取最后一段（同时处理 `/` 与 `\`，防止 Windows 路径混用）；
 * - 去除控制字符 / `:*?"<>|` / 首尾点与空格；
 * - 保留设备名加 `_` 前缀；空名回退 `download`；超长截断。
 */
export function sanitizeDownloadFileName(raw: string): string {
  const base = raw.split(/[\\/]/).pop() ?? "";
  let name = base
    // eslint-disable-next-line no-control-regex -- 控制字符必须剔除（文件名清洗）。
    .replace(/[\u0000-\u001f\u007f]/g, "")
    .replace(/[:*?"<>|]/g, "_")
    .replace(/\.{2,}/g, "_")
    .replace(/^[.\s]+|[.\s]+$/g, "")
    .trim();
  if (name.length === 0 || /^[._]+$/.test(name)) {
    name = "download";
  }
  if (WINDOWS_RESERVED.test(name)) {
    name = `_${name}`;
  }
  if (name.length > MAX_DOWNLOAD_FILE_NAME) {
    const ext = path.extname(name).slice(0, 16);
    const stem = name.slice(0, MAX_DOWNLOAD_FILE_NAME - ext.length);
    name = `${stem}${ext}`;
  }
  return name;
}

/** 在目录内构造不覆盖已有文件的保存路径（`name (1).ext`）。 */
export function uniqueDownloadPath(
  downloadsDir: string,
  fileName: string,
  taken: ReadonlySet<string>,
): string {
  const ext = path.extname(fileName);
  const stem = fileName.slice(0, fileName.length - ext.length);
  let candidate = fileName;
  let index = 1;
  while (taken.has(candidate.toLowerCase())) {
    candidate = `${stem} (${index})${ext}`;
    index += 1;
  }
  return path.join(downloadsDir, candidate);
}

export type BrowserDownloadState = "pending" | "confirmed" | "cancelled";

export interface BrowserDownload {
  id: string;
  url: string;
  fileName: string;
  savePath: string;
  state: BrowserDownloadState;
  totalBytes: number;
  receivedBytes: number;
}

export interface DownloadGateOptions {
  downloadsDir: string;
  /** 已占用文件名（大小写不敏感比较由调用方保证小写）。 */
  taken?: () => ReadonlySet<string>;
  /** 可注入的 id 生成器（测试稳定性）。 */
  nextId?: () => string;
  maxPending?: number;
}

/**
 * 下载确认状态机：`request` 只登记排队，`confirm` 后调用方才允许写盘。
 *
 * `browserHost` 在 `will-download` 中先 `pause()` 并登记；用户确认后 `resume()`。
 */
export class BrowserDownloadGate {
  private readonly downloadsDir: string;
  private readonly taken: () => ReadonlySet<string>;
  private readonly nextId: () => string;
  private readonly maxPending: number;
  private readonly items = new Map<string, BrowserDownload>();
  private counter = 0;

  constructor(options: DownloadGateOptions) {
    this.downloadsDir = options.downloadsDir;
    this.taken = options.taken ?? (() => new Set<string>());
    this.nextId =
      options.nextId ??
      (() => {
        this.counter += 1;
        return `dl-${this.counter}`;
      });
    this.maxPending = options.maxPending ?? MAX_PENDING_DOWNLOADS;
  }

  /** 登记一个待确认下载；队列满返回 `null`（调用方取消下载）。 */
  request(input: {
    url: string;
    suggestedFileName: string;
    totalBytes?: number;
  }): BrowserDownload | null {
    if (this.pending().length >= this.maxPending) {
      return null;
    }
    const fileName = sanitizeDownloadFileName(input.suggestedFileName);
    const savePath = uniqueDownloadPath(this.downloadsDir, fileName, this.taken());
    const item: BrowserDownload = {
      id: this.nextId(),
      url: input.url,
      fileName,
      savePath,
      state: "pending",
      totalBytes: input.totalBytes ?? 0,
      receivedBytes: 0,
    };
    this.items.set(item.id, item);
    return item;
  }

  /** 用户确认：返回保存路径；未找到/已终结返回 `null`。 */
  confirm(id: string): BrowserDownload | null {
    const item = this.items.get(id);
    if (item === undefined || item.state !== "pending") {
      return null;
    }
    item.state = "confirmed";
    return item;
  }

  /** 用户取消：状态转 cancelled；未找到/已终结返回 `null`。 */
  cancel(id: string): BrowserDownload | null {
    const item = this.items.get(id);
    if (item === undefined || item.state !== "pending") {
      return null;
    }
    item.state = "cancelled";
    return item;
  }

  /** 进度更新（只更新已确认项；不改变状态）。 */
  markProgress(id: string, receivedBytes: number, totalBytes?: number): void {
    const item = this.items.get(id);
    if (item === undefined) {
      return;
    }
    item.receivedBytes = Math.max(0, receivedBytes);
    if (totalBytes !== undefined && totalBytes > 0) {
      item.totalBytes = totalBytes;
    }
  }

  /** 全部下载（快照，供诊断/E2E）。 */
  snapshot(): BrowserDownload[] {
    return [...this.items.values()].map((item) => ({ ...item }));
  }

  /** 待确认下载。 */
  pending(): BrowserDownload[] {
    return this.snapshot().filter((item) => item.state === "pending");
  }

  /** 清理已终结记录（保留 pending）。 */
  clearCompleted(): void {
    for (const [id, item] of this.items) {
      if (item.state !== "pending") {
        this.items.delete(id);
      }
    }
  }
}
