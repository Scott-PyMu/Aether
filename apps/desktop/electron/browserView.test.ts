/**
 * WebContentsView 安全策略单测（M9-10 DoD-4；ADR-029 §5.2）。
 */
import path from "node:path";

import { describe, expect, it } from "vitest";

import {
  BROWSER_PARTITION_PREFIX,
  BrowserDownloadGate,
  browserPartition,
  browserWebPreferences,
  decideBrowserNavigation,
  decideBrowserPermission,
  decideBrowserWindowOpen,
  isBrowserContents,
  markBrowserContents,
  sanitizeDownloadFileName,
  uniqueDownloadPath,
} from "./browserView";

describe("browserView security", () => {
  it("每个视图独立 persist partition；非法 id 拒绝", () => {    expect(browserPartition("session-01")).toBe(
      `${BROWSER_PARTITION_PREFIX}session-01`,
    );
    expect(browserPartition("workspace.alpha_2")).toBe(
      `${BROWSER_PARTITION_PREFIX}workspace.alpha_2`,
    );
    for (const invalid of ["", "../x", "a/b", "a b", "-a", "a".repeat(65)]) {
      expect(() => browserPartition(invalid)).toThrow(TypeError);
    }
  });

  it("浏览器 contents 标记：创建后同步登记，统一加固跳过", () => {
    const contents = {};
    expect(isBrowserContents(contents)).toBe(false);
    markBrowserContents(contents);
    expect(isBrowserContents(contents)).toBe(true);
  });

  it("无 preload / 无 Node / sandbox", () => {
    const prefs = browserWebPreferences();
    expect("preload" in prefs).toBe(false);
    expect(prefs.nodeIntegration).toBe(false);
    expect(prefs.nodeIntegrationInWorker).toBe(false);
    expect(prefs.nodeIntegrationInSubFrames).toBe(false);
    expect(prefs.contextIsolation).toBe(true);
    expect(prefs.sandbox).toBe(true);
    expect(prefs.webSecurity).toBe(true);
    expect(prefs.allowRunningInsecureContent).toBe(false);
    expect(prefs.webviewTag).toBe(false);
  });

  it("导航只允许 http/https；file/javascript/data/blob 拒绝", () => {
    expect(decideBrowserNavigation("https://example.com")).toBe("allow");
    expect(decideBrowserNavigation("http://127.0.0.1:8080/x")).toBe("allow");
    for (const blocked of [
      "file:///C:/Windows/win.ini",
      "javascript:alert(1)",
      "data:text/html,<h1>x</h1>",
      "blob:https://example.com/x",
      "chrome://settings",
      "not a url",
    ]) {
      expect(decideBrowserNavigation(blocked)).toBe("deny");
    }
  });

  it("window.open 只把 http/https 交系统浏览器", () => {
    expect(decideBrowserWindowOpen("https://example.com")).toBe("external");
    expect(decideBrowserWindowOpen("http://example.com")).toBe("external");
    expect(decideBrowserWindowOpen("file:///C:/Windows/win.ini")).toBe("deny");
    expect(decideBrowserWindowOpen("javascript:alert(1)")).toBe("deny");
  });

  it("权限默认全部拒绝（M9 不开放任何浏览器能力）", () => {
    for (const permission of [
      "geolocation",
      "notifications",
      "media",
      "clipboard-read",
      "clipboard-sanitized-write",
      "fullscreen",
    ]) {
      expect(decideBrowserPermission(permission)).toBe(false);
    }
  });

  it("下载文件名清洗：路径穿越/控制字符/设备名/超长", () => {
    expect(sanitizeDownloadFileName("..\\..\\evil.txt")).toBe("evil.txt");
    expect(sanitizeDownloadFileName("/etc/passwd")).toBe("passwd");
    expect(sanitizeDownloadFileName("a\u0000b.txt")).toBe("ab.txt");
    expect(sanitizeDownloadFileName('bad:name*?"<>|.txt')).toBe("bad_name______.txt");
    expect(sanitizeDownloadFileName("...")).toBe("download");
    expect(sanitizeDownloadFileName("_.._evil.txt")).toBe("___evil.txt");
    expect(sanitizeDownloadFileName("")).toBe("download");
    expect(sanitizeDownloadFileName("CON")).toBe("_CON");
    expect(sanitizeDownloadFileName("lpt1.txt")).toBe("_lpt1.txt");
    const long = `${"x".repeat(200)}.txt`;
    expect(sanitizeDownloadFileName(long).length).toBeLessThanOrEqual(128);
    expect(sanitizeDownloadFileName(long).endsWith(".txt")).toBe(true);
  });

  it("保存路径不覆盖已有文件", () => {
    const taken = new Set(["report.pdf", "report (1).pdf"]);
    expect(uniqueDownloadPath("C:\\Downloads", "report.pdf", taken)).toBe(
      path.join("C:\\Downloads", "report (2).pdf"),
    );
    expect(uniqueDownloadPath("C:\\Downloads", "new.txt", taken)).toBe(
      path.join("C:\\Downloads", "new.txt"),
    );
  });

  it("下载门：未确认前不得写盘；确认/取消是终态", () => {
    const gate = new BrowserDownloadGate({
      downloadsDir: "C:\\Downloads",
      nextId: (() => {
        let index = 0;
        return () => {
          index += 1;
          return `dl-${index}`;
        };
      })(),
    });
    const first = gate.request({
      url: "https://example.com/a.txt",
      suggestedFileName: "..\\a.txt",
      totalBytes: 10,
    });
    expect(first?.state).toBe("pending");
    expect(first?.savePath).toBe(path.join("C:\\Downloads", "a.txt"));
    expect(gate.confirm(first?.id ?? "")?.state).toBe("confirmed");
    expect(gate.confirm(first?.id ?? "")).toBeNull();
    expect(gate.cancel(first?.id ?? "")).toBeNull();

    const second = gate.request({
      url: "https://example.com/b.txt",
      suggestedFileName: "b.txt",
    });
    expect(second?.state).toBe("pending");
    expect(gate.cancel(second?.id ?? "")?.state).toBe("cancelled");
    expect(gate.pending()).toHaveLength(0);
  });

  it("下载门：队列上限拒绝新下载（避免无界排队）", () => {
    const gate = new BrowserDownloadGate({
      downloadsDir: "C:\\Downloads",
      maxPending: 2,
    });
    expect(
      gate.request({ url: "https://e/1", suggestedFileName: "1.txt" }),
    ).not.toBeNull();
    expect(
      gate.request({ url: "https://e/2", suggestedFileName: "2.txt" }),
    ).not.toBeNull();
    expect(
      gate.request({ url: "https://e/3", suggestedFileName: "3.txt" }),
    ).toBeNull();
  });
});
