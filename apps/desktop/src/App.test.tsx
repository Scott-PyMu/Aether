import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { App } from "./App";

// M2-07：主界面挂载 HealthMonitor（5s 轮询 health）；本文件只验证 M9 主界面骨架，
// 健康查询以永不落定的 Promise 挂起（保持 loading，不触发无响应定时器干扰断言）。
vi.mock("./health", async (importOriginal) => {
  const original = await importOriginal<typeof import("./health")>();
  return {
    ...original,
    fetchHealth: vi.fn().mockReturnValue(new Promise(() => {})),
    requestAppRestart: vi.fn(),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("App（M9-08 主界面）", () => {
  it("直接渲染主界面（M9 无启动门；S-01/S-01a 不可达）", async () => {
    render(<App />);
    expect(await screen.findByTestId("workbench")).toBeTruthy();
    expect(screen.queryByTestId("startup-gate")).toBeNull();
    expect(screen.queryByTestId("startup-loading")).toBeNull();
    expect(screen.queryByTestId("startup-load-error")).toBeNull();
  });

  it("展示版本 / 线协议；不再渲染启动门迁移/退出锚点", async () => {
    render(<App />);
    expect(await screen.findByTestId("app-version")).toBeTruthy();
    expect(screen.getByTestId("app-version").textContent).toBe(
      `版本 ${APP_VERSION}`,
    );
    expect(screen.getByTestId("protocol-version").textContent).toBe(
      `线协议 v${PROTOCOL_VERSION.major}.${PROTOCOL_VERSION.minor}`,
    );
    for (const anchor of [
      "startup-migrate",
      "startup-exit",
      "startup-reasons",
      "startup-pick",
    ]) {
      expect(screen.queryByTestId(anchor)).toBeNull();
    }
  });

  it("主界面挂载健康监控（HealthMonitor）", async () => {
    render(<App />);
    expect(await screen.findByTestId("workbench")).toBeTruthy();
    expect(screen.queryByTestId("core-not-ready-banner")).toBeNull();
  });
});
