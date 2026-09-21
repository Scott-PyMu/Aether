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
