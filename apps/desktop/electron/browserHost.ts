/**
 * `WebContentsView` 宿主（M9-10 DoD-4；ADR-029 D-029-7/§5.2）。
 *
 * 只做 PoC + 安全基线：视图挂到主窗口 `contentView`，不暴露给 renderer、
 * 不加载 preload、不共享 partition/cookie/localStorage，也不与主 UI 共享 IPC。
 * 完整 UI-06（标签/分屏/下载面板/权限提示）属 P4。
 */
import { mkdirSync } from "node:fs";
import path from "node:path";

import {
  BrowserWindow,
  WebContentsView,
  session,
  type DownloadItem,
  type Session as ElectronSession,
} from "electron";

import { discardDownloadedFile, promoteDownloadedFile } from "./downloadFiles";

import {
  BrowserDownloadGate,
  browserPartition,
  browserWebPreferences,
  decideBrowserNavigation,
  decideBrowserWindowOpen,
  type BrowserDownload,
} from "./browserView";
import type { ShellLogger } from "./shellLog";

export interface BrowserBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface BrowserViewSnapshot {
  id: string;
  url: string;
  partition: string;
  title: string;
  loading: boolean;
  loadError: string | null;
  preferences: {
    preload: string | null;
    nodeIntegration: boolean;
    contextIsolation: boolean;
    sandbox: boolean;
    webSecurity: boolean;
    allowRunningInsecureContent: boolean;
    webviewTag: boolean;
  };
}

export interface BrowserHostOptions {
  logger: ShellLogger;
  /** 下载目录（E2E 可指向临时目录；生产 = 系统 Downloads）。 */
  downloadsDir: string;
  /** 当前主窗口（重启保留窗口；被销毁时返回 null）。 */
  getParent: () => BrowserWindow | null;
  /** 外链记录/拉起（复用 main 的 `openExternal`）。 */
  openExternal: (url: string, source: string) => void;
}

interface BrowserEntry {
  id: string;
  view: WebContentsView;
  partition: string;
  loadError: string | null;
}

const DEFAULT_BOUNDS: BrowserBounds = { x: 0, y: 0, width: 800, height: 600 };

export class BrowserHost {
  private readonly logger: ShellLogger;
  private readonly downloadsDir: string;
  private readonly getParent: () => BrowserWindow | null;
  private readonly openExternal: (url: string, source: string) => void;
  private readonly gate: BrowserDownloadGate;
  private readonly entries = new Map<string, BrowserEntry>();
  private readonly configuredSessions = new Set<string>();
  private readonly downloadItems = new Map<string, DownloadItem>();
  private readonly downloadOwner = new Map<string, string>();
  private readonly quarantine = new Map<string, string>();
  private readonly quarantineDir: string;

  constructor(options: BrowserHostOptions) {
    this.logger = options.logger;
    this.downloadsDir = options.downloadsDir;
    this.quarantineDir = path.join(this.downloadsDir, ".quarantine");
    this.getParent = options.getParent;
    this.openExternal = options.openExternal;
    this.gate = new BrowserDownloadGate({
      downloadsDir: this.downloadsDir,
      taken: () => this.takenFileNames(),
    });
  }

  /**
   * 打开一个浏览器视图；同 id 重复打开视为失败（调用方先 close）。
   *
   * 返回 `loadError` 而非抛错：证书错误/网络失败都要能进入快照与日志。
   */
  async open(input: {
    id: string;
    url: string;
    bounds?: BrowserBounds;
  }): Promise<BrowserViewSnapshot | null> {
    if (this.entries.has(input.id)) {
      this.logger.log("warn", "browser.open.duplicate", { id: input.id });
      return null;
    }
    const parent = this.getParent();
    if (parent === null || parent.isDestroyed()) {
      this.logger.log("warn", "browser.open.no_parent", { id: input.id });
      return null;
    }
    if (decideBrowserNavigation(input.url) !== "allow") {
      this.logger.log("warn", "browser.open.blocked_scheme", {
        id: input.id,
        url: input.url,
      });
      return null;
    }

    const partition = browserPartition(input.id);
    const partitionSession = session.fromPartition(partition);
    this.configureSession(partition, partitionSession);

    const view = new WebContentsView({
      webPreferences: { ...browserWebPreferences(), partition },
    });
    const entry: BrowserEntry = {
      id: input.id,
      view,
      partition,
      loadError: null,
    };
    this.entries.set(input.id, entry);
    this.wireContents(entry);

    parent.contentView.addChildView(view);
    view.setBounds(clampBounds(input.bounds ?? DEFAULT_BOUNDS, parent.getContentBounds()));
    this.logger.log("info", "browser.open", {
      id: input.id,
      partition,
      url: input.url,
    });

    try {
      await view.webContents.loadURL(input.url);
    } catch (error) {
      entry.loadError = errorText(error);
      this.logger.log("warn", "browser.load.failed", {
        id: input.id,
        url: input.url,
        error: entry.loadError,
      });
    }
    return this.snapshotOf(entry);
  }

  /** 关闭并销毁视图。 */
  close(id: string): boolean {
    const entry = this.entries.get(id);
    if (entry === undefined) {
      return false;
    }
    this.entries.delete(id);
    for (const [downloadId, owner] of [...this.downloadOwner]) {
      if (owner !== id) {
        continue;
      }
      const record = this.gate.snapshot().find((item) => item.id === downloadId);
      if (record !== undefined && record.state === "pending") {
        this.downloadItems.get(downloadId)?.cancel();
        this.gate.cancel(downloadId);
      }
      this.discardQuarantine(downloadId);
      this.downloadItems.delete(downloadId);
      this.downloadOwner.delete(downloadId);
    }
    try {
      const parent = this.getParent();
      if (parent !== null && !parent.isDestroyed()) {
        parent.contentView.removeChildView(entry.view);
      }
      entry.view.webContents.close();
    } catch {
      // 窗口/视图已销毁。
    }
    this.logger.log("info", "browser.close", { id });
    return true;
  }

  /** 在视图内执行脚本（E2E 隔离/权限断言；生产不暴露给 renderer）。 */
  async evaluate(id: string, script: string): Promise<unknown> {
    const entry = this.entries.get(id);
    if (entry === undefined) {
      return null;
    }
    return entry.view.webContents.executeJavaScript(script, true);
  }

  /** 视图快照（E2E/诊断；不含任何页面数据）。 */
  snapshot(): BrowserViewSnapshot[] {
    return [...this.entries.values()].map((entry) => this.snapshotOf(entry));
  }

  /** 由 main 主动发起下载（E2E 用；生产由页面触发 `will-download`）。 */
  requestDownload(id: string, url: string): boolean {
    const entry = this.entries.get(id);
    if (entry === undefined || decideBrowserNavigation(url) !== "allow") {
      return false;
    }
    entry.view.webContents.downloadURL(url);
    return true;
  }

  /** 用户确认下载：把隔离区文件移动到最终路径（未完成则等 `done`）。 */
  confirmDownload(downloadId: string): boolean {
    const record = this.gate.confirm(downloadId);
    if (record === null) {
      return false;
    }
    this.logger.log("info", "browser.download.confirmed", {
      id: downloadId,
      fileName: record.fileName,
    });
    this.finalizeDownload(downloadId, record);
    return true;
  }

  /** 用户取消下载：取消进行中的下载并清理隔离区。 */
  cancelDownload(downloadId: string): boolean {
    const record = this.gate.cancel(downloadId);
    if (record === null) {
      return false;
    }
    this.downloadItems.get(downloadId)?.cancel();
    this.discardQuarantine(downloadId);
    this.downloadOwner.delete(downloadId);
    this.logger.log("info", "browser.download.cancelled", {
      id: downloadId,
      fileName: record.fileName,
    });
    return true;
  }

  /** 下载快照（状态/文件名/进度；用于 UI 与证据）。 */
  downloads(): ReturnType<BrowserDownloadGate["snapshot"]> {
    return this.gate.snapshot();
  }

  /** 主窗口销毁/应用退出时回收全部视图。 */
  dispose(): void {
    for (const id of [...this.entries.keys()]) {
      this.close(id);
    }
  }

  private configureSession(partition: string, partitionSession: ElectronSession): void {
    if (this.configuredSessions.has(partition)) {
      return;
    }
    this.configuredSessions.add(partition);
    partitionSession.setPermissionRequestHandler((_contents, _permission, callback) => {
      callback(false);
    });
    partitionSession.setPermissionCheckHandler(() => false);
    // -3 = 使用 Chromium 默认校验（证书错误默认拒绝，不提供「继续访问」）。
    partitionSession.setCertificateVerifyProc((_request, callback) => {
      callback(-3);
    });
    partitionSession.on("will-download", (event, item) => {
      const owner = this.ownerForPartition(partition);
      const recorded = this.gate.request({
        url: item.getURL(),
        suggestedFileName: item.getFilename(),
        totalBytes: item.getTotalBytes(),
      });
      if (recorded === null || owner === null) {
        event.preventDefault();
        item.cancel();
        this.logger.log("warn", "browser.download.rejected", {
          url: item.getURL(),
          file: item.getFilename(),
        });
        return;
      }

      // 先写入 downloads/.quarantine 下的隐藏临时文件；确认后才移动到最终路径。
      mkdirSync(this.quarantineDir, { recursive: true });
      const tempPath = path.join(this.quarantineDir, `${recorded.id}.part`);
      item.setSavePath(tempPath);
      this.downloadItems.set(recorded.id, item);
      this.downloadOwner.set(recorded.id, owner);
      this.quarantine.set(recorded.id, tempPath);
      item.once("done", (_event, state) => {
        this.gate.markProgress(recorded.id, item.getReceivedBytes(), item.getTotalBytes());
        const current =
          this.gate.snapshot().find((entry) => entry.id === recorded.id) ?? recorded;
        if (state === "completed" && current.state === "confirmed") {
          this.finalizeDownload(recorded.id, current);
        } else if (state !== "completed") {
          this.discardQuarantine(recorded.id);
        }
        this.downloadItems.delete(recorded.id);
        this.logger.log("info", "browser.download.done", {
          id: recorded.id,
          state,
          fileName: recorded.fileName,
        });
      });
      item.on("updated", (_event, state) => {
        this.gate.markProgress(recorded.id, item.getReceivedBytes(), item.getTotalBytes());
        this.logger.log("debug", "browser.download.updated", {
          id: recorded.id,
          state,
        });
      });
      this.logger.log("info", "browser.download.pending", {
        id: recorded.id,
        url: item.getURL(),
        fileName: recorded.fileName,
        savePath: recorded.savePath,
        totalBytes: recorded.totalBytes,
      });
    });
  }

  private wireContents(entry: BrowserEntry): void {
    const contents = entry.view.webContents;
    contents.setWindowOpenHandler(({ url }) => {
      const decision = decideBrowserWindowOpen(url);
      if (decision === "external") {
        this.openExternal(url, "browser.window.open");
      }
      return { action: "deny" };
    });
    contents.on("will-navigate", (event, url) => {
      if (decideBrowserNavigation(url) === "allow") {
        return;
      }
      event.preventDefault();
      this.logger.log("warn", "browser.navigation.blocked", { id: entry.id, url });
    });
    contents.on("will-attach-webview", (event) => {
      event.preventDefault();
      this.logger.log("warn", "browser.webview.blocked", { id: entry.id });
    });
    contents.on(
      "did-fail-load",
      (_event, errorCode, errorDescription, validatedURL, isMainFrame) => {
        if (!isMainFrame) {
          return;
        }
        entry.loadError = `${errorCode}: ${errorDescription}`;
        this.logger.log("warn", "browser.did_fail_load", {
          id: entry.id,
          errorCode,
          errorDescription,
          validatedURL,
        });
      },
    );
    contents.on("render-process-gone", (_event, details) => {
      this.logger.log("error", "browser.renderer.gone", {
        id: entry.id,
        reason: details.reason,
        exitCode: details.exitCode,
      });
    });
  }

  private snapshotOf(entry: BrowserEntry): BrowserViewSnapshot {
    const contents = entry.view.webContents as unknown as {
      getLastWebPreferences(): {
        preload?: string;
        nodeIntegration?: boolean;
        contextIsolation?: boolean;
        sandbox?: boolean;
        webSecurity?: boolean;
        allowRunningInsecureContent?: boolean;
        webviewTag?: boolean;
      };
    };
    const prefs = contents.getLastWebPreferences();
    return {
      id: entry.id,
      url: entry.view.webContents.getURL(),
      partition: entry.partition,
      title: entry.view.webContents.getTitle(),
      loading: entry.view.webContents.isLoading(),
      loadError: entry.loadError,
      preferences: {
        preload: typeof prefs.preload === "string" ? prefs.preload : null,
        nodeIntegration: prefs.nodeIntegration === true,
        contextIsolation: prefs.contextIsolation === true,
        sandbox: prefs.sandbox === true,
        webSecurity: prefs.webSecurity === true,
        allowRunningInsecureContent: prefs.allowRunningInsecureContent === true,
        webviewTag: prefs.webviewTag === true,
      },
    };
  }

  private finalizeDownload(downloadId: string, record: BrowserDownload): void {
    const tempPath = this.quarantine.get(downloadId);
    if (tempPath === undefined) {
      return;
    }
    const item = this.downloadItems.get(downloadId);
    if (item !== undefined && item.getState() !== "completed") {
      // 仍在下行；`done` 回调会再次调用本方法。
      return;
    }
    const result = promoteDownloadedFile(tempPath, record.savePath);
    if (result.ok) {
      this.logger.log("info", "browser.download.saved", {
        id: downloadId,
        savePath: record.savePath,
      });
    } else {
      this.logger.log("error", "browser.download.save_failed", {
        id: downloadId,
        error: result.error ?? "unknown",
      });
    }
    this.quarantine.delete(downloadId);
  }

  private discardQuarantine(downloadId: string): void {
    const tempPath = this.quarantine.get(downloadId);
    if (tempPath === undefined) {
      return;
    }
    this.quarantine.delete(downloadId);
    discardDownloadedFile(tempPath);
  }

  private ownerForPartition(partition: string): string | null {
    return (
      [...this.entries.values()].find((entry) => entry.partition === partition)?.id ??
      null
    );
  }

  private takenFileNames(): ReadonlySet<string> {
    return new Set(
      this.gate
        .snapshot()
        .filter((item) => item.state !== "cancelled")
        .map((item) => item.fileName.toLowerCase()),
    );
  }
}

function clampBounds(
  bounds: BrowserBounds,
  container: { width: number; height: number },
): BrowserBounds {
  const x = Math.max(0, Math.min(bounds.x, container.width));
  const y = Math.max(0, Math.min(bounds.y, container.height));
  return {
    x,
    y,
    width: Math.max(0, Math.min(bounds.width, container.width - x)),
    height: Math.max(0, Math.min(bounds.height, container.height - y)),
  };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
