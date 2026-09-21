import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";

import {
  fetchHealth,
  HEALTH_POLL_INTERVAL_MS,
  HEALTH_TIMEOUT_MS,
  requestAppRestart,
  type HealthReport,
} from "./health";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const normalReport: HealthReport = {
  storage_state: "normal",
  write_queue_depth: 3,
  runtimes: [{ id: "mock", status: "ready" }],
  ts: 1_758_000_000_000,
};

beforeEach(() => {
  invokeMock.mockReset();
});

describe("health IPC 契约（ADR-007 附录 A）", () => {
  it("fetchHealth 调用无参数 `health` 并透传报告形状", async () => {
    invokeMock.mockResolvedValue(normalReport);
    const report = await fetchHealth();
    expect(invokeMock).toHaveBeenCalledWith("health");
    expect(report).toEqual(normalReport);
    expect(report.runtimes?.[0]?.status).toBe("ready");
  });

  it("runtimes 允许 null（监督器未接线）与 []（已接线无 runtime）", async () => {
    invokeMock.mockResolvedValue({ ...normalReport, runtimes: null });
    expect((await fetchHealth()).runtimes).toBeNull();
    invokeMock.mockResolvedValue({ ...normalReport, runtimes: [] });
    expect((await fetchHealth()).runtimes).toEqual([]);
  });

  it("requestAppRestart 复用 app_restart 且显式 confirm:true（ADR-004）", async () => {
    invokeMock.mockResolvedValue(undefined);
    await requestAppRestart();
    expect(invokeMock).toHaveBeenCalledWith("app_restart", {
      payload: { confirm: true },
    });
  });

  it("轮询与超时口径为 D2 常量（5s / 15s）", () => {
    expect(HEALTH_POLL_INTERVAL_MS).toBe(5_000);
    expect(HEALTH_TIMEOUT_MS).toBe(15_000);
  });
});
