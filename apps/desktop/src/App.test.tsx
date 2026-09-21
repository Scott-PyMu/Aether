import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { App } from "./App";
import { fetchStartup, type StartupSnapshot } from "./startup";

vi.mock("./startup", async (importOriginal) => {
  const original = await importOriginal<typeof import("./startup")>();
  return { ...original, fetchStartup: vi.fn() };
});

// M2-07：主界面挂载 HealthMonitor（5s 轮询 health）；本文件只验证启动门/骨架，
// 健康查询以永不落定的 Promise 挂起（保持 loading，不触发无响应定时器干扰断言）。
vi.mock("./health", async (importOriginal) => {
  const original = await importOriginal<typeof import("./health")>();
  return {
    ...original,
    fetchHealth: vi.fn().mockReturnValue(new Promise(() => {})),
    requestAppRestart: vi.fn(),
  };
});

const fetchStartupMock = vi.mocked(fetchStartup);

const readySnapshot: StartupSnapshot = {
  phase: "ready",
  data_dir: "C:\\Local\\Aether",
  data_dir_source: "default",
};

const blockedSnapshot: StartupSnapshot = {
  phase: "blocked_sync_dir",
  data_dir: "C:\\Users\\me\\OneDrive\\Aether",
  data_dir_source: "default",
  detection: {
    platform: "windows",
    candidate: "C:\\Users\\me\\OneDrive\\Aether",
    resolved: "C:\\Users\\me\\OneDrive\\Aether",
    verdict: "reject",
    checks: [
      {
        id: "win.one_drive_env_prefix",
        label: "OneDrive 环境变量前缀祖先",
        hit: true,
        detail: "命中 OneDrive=C:\\Users\\me\\OneDrive",
        precision: "exact",
      },
    ],
    reasons: ["OneDrive 环境变量前缀祖先：命中 OneDrive=C:\\Users\\me\\OneDrive"],
    note: "macOS 同步盘检测精度受限（iCloud 采用路径前缀近似，未绑定 NSURLIsUbiquitousItemKey）；请确认目录不在 iCloud/CloudStorage 下。",
  },
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("App", () => {
  it("启动自检通过时显示主界面（版本号 / 线协议 / 数据目录）", async () => {
    fetchStartupMock.mockResolvedValue(readySnapshot);
    render(<App />);
    expect(screen.getByTestId("startup-loading")).toBeTruthy();
    expect(await screen.findByTestId("app-version")).toBeTruthy();
    expect(screen.getByTestId("app-version").textContent).toBe(
      `版本 ${APP_VERSION}`,
    );
    expect(screen.getByTestId("protocol-version").textContent).toBe(
      `线协议 v${PROTOCOL_VERSION.major}.${PROTOCOL_VERSION.minor}`,
    );
    expect(screen.getByTestId("app-data-dir").textContent).toContain(
      readySnapshot.data_dir,
    );
  });

  it("检测命中同步盘时主界面不可达，仅渲染启动门", async () => {
    fetchStartupMock.mockResolvedValue(blockedSnapshot);
    render(<App />);
    expect(await screen.findByTestId("startup-gate")).toBeTruthy();
    expect(screen.queryByTestId("app-version")).toBeNull();
    expect(screen.getByTestId("startup-migrate")).toBeTruthy();
    expect(screen.getByTestId("startup-exit")).toBeTruthy();
    expect(screen.getByTestId("startup-reasons").textContent).toContain(
      "OneDrive 环境变量前缀祖先",
    );
    expect(screen.getByTestId("startup-precision-note").textContent).toContain(
      "检测精度受限",
    );
    expect(screen.getByTestId("startup-precision-note").textContent).toContain(
      "请确认目录不在 iCloud/CloudStorage 下",
    );
  });

  it("启动自检调用失败时展示结构化错误信息", async () => {
    fetchStartupMock.mockRejectedValue({ code: "internal", message: "快照失败" });
    render(<App />);
    expect(await screen.findByTestId("startup-load-error")).toBeTruthy();
    expect(screen.getByTestId("startup-load-error").textContent).toContain(
      "快照失败",
    );
  });
});
