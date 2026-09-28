import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { HealthMonitor } from "./HealthMonitor";
import { requestAppRestart } from "./health";
import { useHealthPolling, type HealthState } from "./useHealthPolling";

vi.mock("./useHealthPolling", () => ({ useHealthPolling: vi.fn() }));
vi.mock("./health", async (importOriginal) => {
  const original = await importOriginal<typeof import("./health")>();
  return { ...original, requestAppRestart: vi.fn() };
});

const pollingMock = vi.mocked(useHealthPolling);
const restartMock = vi.mocked(requestAppRestart);

const state = (overrides: Partial<HealthState>): HealthState => ({
  status: "loading",
  report: null,
  error: null,
  errorCode: null,
  ...overrides,
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("HealthMonitor（M2-07 DoD4/DoD5）", () => {
  it("无响应态：显示「核心未响应」+ 重启入口，点击触发 app_restart", async () => {
    pollingMock.mockReturnValue(
      state({ status: "unresponsive", error: "IPC 超时" }),
    );
    restartMock.mockResolvedValue(undefined);
    render(<HealthMonitor />);

    const banner = screen.getByTestId("core-unresponsive");
    expect(banner.textContent).toContain("核心未响应");
    expect(banner.textContent).toContain("15s");
    expect(screen.getByTestId("core-restart")).toBeTruthy();

    fireEvent.click(screen.getByTestId("core-restart"));
    await vi.waitFor(() => {
      expect(restartMock).toHaveBeenCalledTimes(1);
    });
  });

  it("重启入口失败时展示结构化错误", async () => {
    pollingMock.mockReturnValue(state({ status: "unresponsive" }));
    restartMock.mockRejectedValue({ code: "not_implemented", message: "M3-06 未实现" });
    render(<HealthMonitor />);

    fireEvent.click(screen.getByTestId("core-restart"));
    expect(await screen.findByTestId("core-restart-error")).toBeTruthy();
    expect(screen.getByTestId("core-restart-error").textContent).toContain(
      "M3-06 未实现",
    );
  });

  it("降级态：只读横幅展示触发源与 detail", () => {
    pollingMock.mockReturnValue(
      state({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          degrade_trigger: "write_failure",
          detail: "连续 3 次写事务尝试失败",
        },
      }),
    );
    render(<HealthMonitor />);
    const banner = screen.getByTestId("storage-degraded");
    expect(banner.textContent).toContain("存储降级（只读）");
    expect(banner.textContent).toContain("write_failure");
    expect(banner.textContent).toContain("连续 3 次写事务尝试失败");
  });

  // M3-06 DoD3：降级恢复引导仅 app_restart（修复外部条件 + 重启核心 + 启动自检；
  // 无「一键恢复/热恢复」按钮，D4/ADR-004）。
  it("降级态：提供 app_restart 恢复入口（点击触发）且无热恢复入口", async () => {
    pollingMock.mockReturnValue(
      state({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          degrade_trigger: "space_guard",
        },
      }),
    );
    restartMock.mockResolvedValue(undefined);
    render(<HealthMonitor />);

    const banner = screen.getByTestId("storage-degraded");
    expect(banner.textContent).toContain("运行中的任务已中断");
    expect(screen.getByTestId("storage-degraded-hint").textContent).toContain("无热恢复");
    const restart = screen.getByTestId("storage-degraded-restart");
    expect(banner.textContent).not.toContain("一键恢复");

    fireEvent.click(restart);
    await vi.waitFor(() => {
      expect(restartMock).toHaveBeenCalledTimes(1);
    });
  });

  // M3-06：启动序列过渡窗口（ADR-007 增量 2）——`health` 返回 `core_not_ready`
  // 时展示 core-not-ready-banner（按结构化错误码判别），后端注入后自动消失。
  it("过渡窗口：core_not_ready → core-not-ready-banner", () => {
    pollingMock.mockReturnValue(
      state({
        status: "loading",
        error: "核心后端未就绪：启动序列尚未完成（存储/管线注入前）",
        errorCode: "core_not_ready",
      }),
    );
    const { unmount } = render(<HealthMonitor />);
    const banner = screen.getByTestId("core-not-ready-banner");
    expect(banner.textContent).toContain("核心启动中");
    expect(screen.queryByTestId("health-loading")).toBeNull();
    unmount();

    // 其他错误（无结构化码）：保持通用加载态，不误报过渡横幅。
    pollingMock.mockReturnValue(state({ status: "loading", error: "IPC 失败" }));
    render(<HealthMonitor />);
    expect(screen.queryByTestId("core-not-ready-banner")).toBeNull();
    expect(screen.getByTestId("health-loading")).toBeTruthy();
  });

  it("正常态与加载态锚点（E2E 断言用）", () => {
    pollingMock.mockReturnValue(state({ status: "normal" }));
    const { unmount } = render(<HealthMonitor />);
    expect(screen.getByTestId("health-normal").textContent).toContain(
      "storage_state=normal",
    );
    unmount();

    pollingMock.mockReturnValue(state({ status: "loading" }));
    render(<HealthMonitor />);
    expect(screen.getByTestId("health-loading")).toBeTruthy();
  });
});
