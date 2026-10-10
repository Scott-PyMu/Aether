/**
 * M5-02 前端集成测试：自动备份（DoD5 UI 面）。
 *
 * 覆盖：
 * - S-05 设置页：自动备份开关 / 频率 / 保留份数（`settings-backup-auto-*`）
 *   读取（settings_get）与保存（settings_set 三键）；保留份数非法值校验；
 * - S-06 备份页：`backup-auto-status`（开关/频率/保留/最近运行/下次排期）、
 *   `backup-item[data-kind=manual|auto]`、清单头保留份数；
 * - 7 天提醒联动：自动备份成功后（`reminder.due=false`）不渲染提醒横幅
 *   （沿用单一提醒源，不重复告警）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { BackupPage } from "./BackupPage";
import type { BackupAutoSegment, BackupIpc, BackupListResponse } from "./backup";
import { SettingsPage } from "./SettingsPage";
import {
  BACKUP_AUTO_ENABLED_KEY,
  BACKUP_AUTO_FREQUENCY_KEY,
  BACKUP_AUTO_RETENTION_KEY,
  type SettingsIpc,
} from "./settings";

/** 负载下（`pnpm -r test` 并行）放宽等待预算；断言不变。 */
const WAIT = { timeout: 5000 };

const AUTO_OK: BackupAutoSegment = {
  enabled: true,
  frequency: "daily",
  retention: 5,
  last_run_at: 1_700_000_000_000,
  last_result: "success",
  consecutive_failures: 0,
  next_run_at: 1_700_086_400_000,
};

function listResponse(auto: BackupAutoSegment | null, reminderDue = false): BackupListResponse {
  return {
    backups: [
      {
        id: "01J00000000000000000000A01",
        path: "C:\\Local\\Aether\\backups\\aether-1.db",
        size_bytes: 2048,
        encrypted: false,
        kind: "auto",
        created_at: 1_700_000_000_000,
      },
      {
        id: "01J00000000000000000000M01",
        path: "E:\\external\\aether-2.db",
        size_bytes: 4096,
        encrypted: false,
        kind: "manual",
        created_at: 1_700_000_001_000,
      },
    ],
    capacity: {
      db_bytes: 1024,
      wal_bytes: 1024,
      total_bytes: 2048,
      warn_bytes: 2 * 1024 * 1024 * 1024,
      critical_bytes: 5 * 1024 * 1024 * 1024,
      level: "ok",
    },
    reminder: {
      enabled: true,
      due: reminderDue,
      last_backup_at: reminderDue ? null : 1_700_000_000_000,
      since_ms: reminderDue ? null : 1_000,
      threshold_ms: 7 * 24 * 60 * 60 * 1000,
      reason: reminderDue ? "never" : null,
    },
    ...(auto ? { auto } : {}),
  };
}

function fakeBackupIpc(options: {
  auto?: BackupAutoSegment | null;
  reminderDue?: boolean;
} = {}): BackupIpc {
  const auto = options.auto === undefined ? AUTO_OK : options.auto;
  return {
    list: vi.fn().mockResolvedValue(listResponse(auto, options.reminderDue ?? false)),
    create: vi.fn(),
    restore: vi.fn(),
    pickTargetDir: vi.fn().mockResolvedValue(null),
  };
}

function fakeSettingsIpc(values: Partial<Record<string, unknown>> = {}): SettingsIpc {
  const store: Record<string, unknown> = {
    [BACKUP_AUTO_ENABLED_KEY]: true,
    [BACKUP_AUTO_FREQUENCY_KEY]: "daily",
    [BACKUP_AUTO_RETENTION_KEY]: 10,
    ...values,
  };
  return {
    get: vi
      .fn()
      .mockImplementation(async (key: string) => ({ key, value: store[key] ?? null })),
    set: vi.fn().mockImplementation(async (key: string, value: unknown) => {
      store[key] = value;
      return { key, value };
    }),
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("BackupPage 自动备份状态（M5-02）", () => {
  it("渲染 auto 段（开关/频率/保留/最近运行/下次排期）与 kind 标签", async () => {
    render(
      <BackupPage
        ipc={fakeBackupIpc()}
        settings={fakeSettingsIpc()}
        onBack={() => {}}
      />,
    );

    const status = await screen.findByTestId("backup-auto-status", {}, WAIT);
    expect(status.getAttribute("data-enabled")).toBe("true");
    expect(status.getAttribute("data-last-result")).toBe("success");
    expect(status.getAttribute("data-failures")).toBe("0");
    expect(status.textContent).toContain("已启用");
    expect(status.textContent).toContain("每天");
    expect(status.textContent).toContain("保留 5 份");
    expect(status.textContent).toContain("成功");

    const items = await screen.findAllByTestId("backup-item", {}, WAIT);
    expect(items[0]!.getAttribute("data-kind")).toBe("auto");
    expect(items[1]!.getAttribute("data-kind")).toBe("manual");
    expect(screen.getByText(/备份清单（自动产物保留 5 份；手动产物不自动删除）/)).toBeTruthy();
  });

  it("失败结果展示连续失败次数（data-failures + role=alert）", async () => {
    render(
      <BackupPage
        ipc={fakeBackupIpc({
          auto: {
            ...AUTO_OK,
            last_result: "failed",
            consecutive_failures: 2,
            enabled: false,
          },
        })}
        settings={fakeSettingsIpc()}
        onBack={() => {}}
      />,
    );
    const status = await screen.findByTestId("backup-auto-status", {}, WAIT);
    expect(status.getAttribute("data-last-result")).toBe("failed");
    expect(status.getAttribute("data-failures")).toBe("2");
    expect(status.getAttribute("role")).toBe("alert");
    expect(status.textContent).toContain("失败，连续 2 次");
    expect(status.textContent).toContain("已停用");
  });

  it("自动备份成功（提醒 due=false）→ 不渲染提醒横幅（不重复告警）", async () => {
    render(
      <BackupPage
        ipc={fakeBackupIpc({ reminderDue: false })}
        settings={fakeSettingsIpc()}
        onBack={() => {}}
      />,
    );
    await screen.findByTestId("backup-auto-status", {}, WAIT);
    expect(screen.queryByTestId("backup-reminder")).toBeNull();
  });

  it("无 auto 段（旧后端）时保持渲染不回归", async () => {
    render(
      <BackupPage
        ipc={fakeBackupIpc({ auto: null })}
        settings={fakeSettingsIpc()}
        onBack={() => {}}
      />,
    );
    await screen.findByTestId("backup-list", {}, WAIT);
    expect(screen.queryByTestId("backup-auto-status")).toBeNull();
    expect(screen.getByText(/备份清单（自动产物保留 10 份；手动产物不自动删除）/)).toBeTruthy();
  });
});

describe("SettingsPage 自动备份设置（M5-02）", () => {
  it("读取设置并保存（三键 settings_set）", async () => {
    const ipc = fakeSettingsIpc({
      [BACKUP_AUTO_ENABLED_KEY]: true,
      [BACKUP_AUTO_FREQUENCY_KEY]: "daily",
      [BACKUP_AUTO_RETENTION_KEY]: 5,
    });
    render(<SettingsPage dataDir="C:\\Local\\Aether" ipc={ipc} onBack={() => {}} />);

    const toggle = await screen.findByTestId("settings-backup-auto-enabled", {}, WAIT);
    await waitFor(() => {
      expect(toggle).toHaveProperty("checked", true);
    }, WAIT);
    expect(screen.getByTestId("settings-backup-auto-frequency")).toHaveProperty(
      "value",
      "daily",
    );
    const retention = screen.getByTestId("settings-backup-auto-retention");
    await waitFor(() => {
      expect(retention).toHaveProperty("value", "5");
    }, WAIT);

    fireEvent.click(toggle); // 关闭
    fireEvent.change(screen.getByTestId("settings-backup-auto-frequency"), {
      target: { value: "weekly" },
    });
    fireEvent.change(retention, { target: { value: "20" } });
    fireEvent.click(screen.getByTestId("settings-backup-auto-save"));

    await waitFor(() => {
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_AUTO_ENABLED_KEY, false);
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_AUTO_FREQUENCY_KEY, "weekly");
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_AUTO_RETENTION_KEY, 20);
    }, WAIT);
    expect(screen.getByTestId("settings-notice").textContent).toContain("自动备份设置已保存");
  });

  it("保留份数非法（0）→ out_of_range 且不调用 settings_set", async () => {
    const ipc = fakeSettingsIpc();
    render(<SettingsPage dataDir="C:\\Local\\Aether" ipc={ipc} onBack={() => {}} />);
    const retention = await screen.findByTestId("settings-backup-auto-retention", {}, WAIT);
    await waitFor(() => {
      expect(retention).toHaveProperty("value", "10");
    }, WAIT);

    fireEvent.change(retention, { target: { value: "0" } });
    fireEvent.click(screen.getByTestId("settings-backup-auto-save"));

    const error = await screen.findByTestId("settings-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("out_of_range");
    expect(ipc.set).not.toHaveBeenCalled();
  });
});
