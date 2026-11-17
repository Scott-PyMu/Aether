/**
 * M10-10 / ADR-039 v1.0 D-039-5：启动校验进度、检查失败 S-08、维护态 S-06。
 *
 * UI-UX v0.20：校验中显示 S-00（x/8 + 主库校验中/完成），不显示 S-08；
 * 主库校验失败使用独立文案；维护态不是降级；S-06 展示维护原因。
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { BackupPage } from "./BackupPage";
import type { BackupIpc } from "./backup";
import { HealthMonitor } from "./HealthMonitor";
import { useHealthPolling, type HealthState } from "./useHealthPolling";

vi.mock("./useHealthPolling", () => ({ useHealthPolling: vi.fn() }));
vi.mock("./health", async (importOriginal) => {
  const original = await importOriginal<typeof import("./health")>();
  return { ...original, requestAppRestart: vi.fn() };
});

const pollingMock = vi.mocked(useHealthPolling);

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

describe("M10-10 存储校验/维护 UI（S-00/S-06/S-08）", () => {
  it("校验中：显示 x/8 与主库校验中，不显示 S-08", () => {
    pollingMock.mockReturnValue(
      state({
        status: "normal",
        report: {
          storage_state: "normal",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          storage_check: {
            state: "checking",
            mode: "startup",
            write_gate: "checking",
            main_checked: false,
            checked_shards: 3,
            total_shards: 8,
          },
        },
      }),
    );
    render(<HealthMonitor />);
    const progress = screen.getByTestId("storage-check-progress");
    expect(progress.textContent).toContain("3/8");
    expect(progress.textContent).toContain("主库校验中");
    expect(progress.getAttribute("data-state")).toBe("checking");
    expect(progress.getAttribute("data-main-checked")).toBe("0");
    expect(screen.queryByTestId("storage-degraded")).toBeNull();
    expect(screen.queryByTestId("health-normal")).toBeNull();
  });

  it("校验中优先于降级状态：不显示 S-08", () => {
    pollingMock.mockReturnValue(
      state({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          storage_check: {
            state: "checking",
            mode: "startup",
            write_gate: "checking",
            main_checked: true,
            checked_shards: 5,
            total_shards: 8,
          },
        },
      }),
    );
    render(<HealthMonitor />);
    expect(screen.getByTestId("storage-check-progress").textContent).toContain(
      "主库校验完成",
    );
    expect(screen.queryByTestId("storage-degraded")).toBeNull();
  });

  it("主库校验失败：S-08 使用独立文案与 data-source", () => {
    pollingMock.mockReturnValue(
      state({
        status: "degraded",
        report: {
          storage_state: "persist_degraded",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          storage_check: {
            state: "failed",
            mode: "startup",
            write_gate: "checking",
            main_checked: false,
            checked_shards: 0,
            total_shards: 8,
            error: "quick_check 失败：主库损坏",
          },
        },
      }),
    );
    render(<HealthMonitor />);
    const banner = screen.getByTestId("storage-degraded");
    expect(banner.getAttribute("data-source")).toBe("storage_check");
    expect(banner.textContent).toContain("主库校验失败");
    expect(banner.textContent).toContain("quick_check 失败：主库损坏");
  });

  it("维护态：显示 S-06 维护提示，不显示降级横幅", () => {
    pollingMock.mockReturnValue(
      state({
        status: "normal",
        report: {
          storage_state: "normal",
          write_queue_depth: 0,
          runtimes: null,
          ts: 1,
          maintenance: { kind: "backup", started_at: 1_700_000_000_000 },
        },
      }),
    );
    render(<HealthMonitor />);
    const banner = screen.getByTestId("storage-maintenance");
    expect(banner.getAttribute("data-kind")).toBe("backup");
    expect(banner.textContent).toContain("维护中");
    expect(screen.queryByTestId("storage-degraded")).toBeNull();
  });

  it("S-06 备份页展示维护态横幅", async () => {
    const ipc = {
      list: vi.fn().mockResolvedValue({
        backups: [],
        capacity: {
          db_bytes: 0,
          wal_bytes: 0,
          total_bytes: 0,
          warn_bytes: 2 * 1024 * 1024 * 1024,
          critical_bytes: 5 * 1024 * 1024 * 1024,
          level: "ok",
        },
      }),
      create: vi.fn(),
      restore: vi.fn(),
      pickTargetDir: vi.fn(),
    } as unknown as BackupIpc;
    render(
      <BackupPage
        ipc={ipc}
        maintenance={{ kind: "restore", started_at: 1_700_000_000_000 }}
        onBack={() => {}}
      />,
    );
    const banner = await screen.findByTestId("backup-maintenance");
    expect(banner.getAttribute("data-kind")).toBe("restore");
    expect(banner.textContent).toContain("restore");
  });
});
