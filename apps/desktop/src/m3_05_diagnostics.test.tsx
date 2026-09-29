/**
 * M3-05 前端集成测试（S-07 诊断导出 / S-05 设置 / S-10 关于 / 右栏诊断入口 / 备份提醒）。
 *
 * 覆盖 DoD：
 * - DoD2：容量阈值参数化模拟 → 警告/强提示横幅（`diagnostics-capacity` / `data-level`）；
 * - DoD3：7 天未备份提醒可开关（`backup-reminder` + `backup-reminder-dismiss` →
 *   `settings_set("backup.reminder", false)`）；
 * - DoD1/DoD4 的 UI 面：导出回执展示 `scanned_clean` 守门结果。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AboutPage } from "./AboutPage";
import { App } from "./App";
import { BackupPage } from "./BackupPage";
import type { BackupIpc, BackupListResponse } from "./backup";
import { DiagnosticsEntry } from "./DiagnosticsEntry";
import { DiagnosticsPage } from "./DiagnosticsPage";
import type { DiagnosticsIpc } from "./diagnostics";
import { SettingsPage } from "./SettingsPage";
import {
  BACKUP_REMINDER_KEY,
  type BackupReminder,
  type SettingsIpc,
} from "./settings";
import { fetchStartup, type StartupSnapshot } from "./startup";

vi.mock("./startup", async (importOriginal) => {
  const original = await importOriginal<typeof import("./startup")>();
  return { ...original, fetchStartup: vi.fn() };
});

vi.mock("./health", async (importOriginal) => {
  const original = await importOriginal<typeof import("./health")>();
  return {
    ...original,
    fetchHealth: vi.fn().mockReturnValue(new Promise(() => {})),
    requestAppRestart: vi.fn(),
  };
});

const CAPACITY = {
  db_bytes: 1024,
  wal_bytes: 1024,
  total_bytes: 2048,
  warn_bytes: 2 * 1024 * 1024 * 1024,
  critical_bytes: 5 * 1024 * 1024 * 1024,
  level: "warn" as const,
};

const REMINDER_DUE: BackupReminder = {
  enabled: true,
  due: true,
  last_backup_at: null,
  since_ms: null,
  threshold_ms: 7 * 24 * 60 * 60 * 1000,
  reason: "never",
};

const LIST_RESPONSE: BackupListResponse = {
  backups: [],
  capacity: CAPACITY,
  reminder: REMINDER_DUE,
};

function fakeDiagnosticsIpc(overrides: Partial<DiagnosticsIpc> = {}): DiagnosticsIpc {
  return {
    exportDiagnostics: vi.fn().mockResolvedValue({
      path: "E:\\diag\\aether-diagnostics-1.json",
      file_name: "aether-diagnostics-1.json",
      bytes: 1234,
      generated_at: 1,
      sections: ["app", "health", "capacity", "logs", "task_dumps"],
      scanned_clean: true,
      log_lines: 3,
      task_dumps: 1,
    }),
    pickTargetDir: vi.fn().mockResolvedValue("E:\\diag"),
    capacity: vi.fn().mockResolvedValue(CAPACITY),
    ...overrides,
  };
}

function fakeSettingsIpc(initial: unknown = true): SettingsIpc {
  return {
    get: vi.fn().mockResolvedValue({ key: BACKUP_REMINDER_KEY, value: initial }),
    set: vi.fn().mockImplementation(async (key: string, value: unknown) => ({ key, value })),
  };
}

function fakeBackupIpc(overrides: Partial<BackupIpc> = {}): BackupIpc {
  return {
    list: vi.fn().mockResolvedValue(LIST_RESPONSE),
    create: vi.fn(),
    restore: vi.fn(),
    pickTargetDir: vi.fn().mockResolvedValue(null),
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("DiagnosticsPage（S-07）", () => {
  it("容量横幅（data-level）+ 选择目录 + 导出回执（scanned_clean）", async () => {
    const ipc = fakeDiagnosticsIpc();
    render(<DiagnosticsPage ipc={ipc} onBack={() => {}} />);

    const capacity = await screen.findByTestId("diagnostics-capacity", {}, WAIT);
    expect(capacity.getAttribute("data-level")).toBe("warn");
    expect(capacity.textContent).toContain("容量警告");

    fireEvent.click(screen.getByTestId("diagnostics-pick"));
    await waitFor(() => {
      expect(screen.getByTestId("diagnostics-target")).toHaveProperty(
        "value",
        "E:\\diag",
      );
    }, WAIT);

    fireEvent.click(screen.getByTestId("diagnostics-export"));
    const result = await screen.findByTestId("diagnostics-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain("aether-diagnostics-1.json");
    expect(result.textContent).toContain("脱敏守门 0 命中");
    expect(ipc.exportDiagnostics).toHaveBeenCalledWith("E:\\diag");
  });

  it("导出失败展示结构化错误（data-result=failed + data-code）", async () => {
    const ipc = fakeDiagnosticsIpc({
      pickTargetDir: vi.fn().mockResolvedValue("E:\\diag"),
      exportDiagnostics: vi
        .fn()
        .mockRejectedValue({ code: "invalid_value", message: "目标可用空间不足" }),
    });
    render(<DiagnosticsPage ipc={ipc} onBack={() => {}} />);
    fireEvent.click(screen.getByTestId("diagnostics-pick"));
    await waitFor(() => {
      expect(screen.getByTestId("diagnostics-export")).toHaveProperty("disabled", false);
    }, WAIT);
    fireEvent.click(screen.getByTestId("diagnostics-export"));

    const result = await screen.findByTestId("diagnostics-result", {}, WAIT);
    expect(result.getAttribute("data-result")).toBe("failed");
    const error = screen.getByTestId("diagnostics-error");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("目标可用空间不足");
  });

  it("容量强提示阈值模拟（critical → role=alert）", async () => {
    const ipc = fakeDiagnosticsIpc({
      capacity: vi.fn().mockResolvedValue({
        ...CAPACITY,
        total_bytes: 6 * 1024 * 1024 * 1024,
        level: "critical",
      }),
    });
    render(<DiagnosticsPage ipc={ipc} onBack={() => {}} />);
    const capacity = await screen.findByTestId("diagnostics-capacity", {}, WAIT);
    expect(capacity.getAttribute("data-level")).toBe("critical");
    expect(capacity.getAttribute("role")).toBe("alert");
    expect(capacity.textContent).toContain("容量紧张");
  });
});

const DATA_DIR = "C:\\Local\\Aether";

/** 负载下（`pnpm -r test` 并行）放宽等待预算；断言不变。 */
const WAIT = { timeout: 5000 };

describe("SettingsPage（S-05）", () => {
  it("备份提醒开关读写（settings_get/set）与安全级别 data-level", async () => {
    const ipc = fakeSettingsIpc(false);
    render(
      <SettingsPage
        dataDir={DATA_DIR}
        securityLevel={{ level: "degraded", detail: "注入：凭据库不可用" }}
        ipc={ipc}
        onBack={() => {}}
      />,
    );

    const toggle = await screen.findByTestId("settings-backup-reminder", {}, WAIT);
    await waitFor(() => {
      expect(toggle).toHaveProperty("checked", false);
    }, WAIT);
    expect(screen.getByTestId("settings-data-dir").textContent).toContain(DATA_DIR);
    const level = screen.getByTestId("settings-security-level");
    expect(level.getAttribute("data-level")).toBe("degraded");
    expect(level.getAttribute("role")).toBe("alert");

    fireEvent.click(toggle);
    await waitFor(() => {
      expect(ipc.set).toHaveBeenCalledWith(BACKUP_REMINDER_KEY, true);
    }, WAIT);
    expect(screen.getByTestId("settings-notice").textContent).toContain("已开启");
  });

  it("未探测安全级别时显示 unknown（启动快照缺省）", () => {
    render(
      <SettingsPage dataDir="C:\\Local\\Aether" ipc={fakeSettingsIpc()} onBack={() => {}} />,
    );
    expect(screen.getByTestId("settings-security-level").getAttribute("data-level")).toBe(
      "unknown",
    );
  });
});

describe("AboutPage（S-10）", () => {
  it("版本/线协议/安全边界锚点齐备；real-adapter 路径不显示 Mock-only 标记", () => {
    render(<AboutPage dataDir="C:\\Local\\Aether" onBack={() => {}} />);
    expect(screen.getByTestId("about-version").textContent).toContain("版本");
    expect(screen.getByTestId("about-protocol").textContent).toContain("线协议");
    expect(screen.getByTestId("about-security-boundary").textContent).toContain(
      "信任级",
    );
    expect(screen.queryByTestId("about-beta-marker")).toBeNull();
  });

  it("mock-only 路径条件显示标记", () => {
    render(<AboutPage dataDir="C:\\Local\\Aether" mockOnly onBack={() => {}} />);
    expect(screen.getByTestId("about-beta-marker").textContent).toContain(
      "Mock-only beta",
    );
  });
});

describe("DiagnosticsEntry（右栏路由入口）", () => {
  it("渲染容量等级并触发打开回调", async () => {
    const onOpen = vi.fn();
    render(<DiagnosticsEntry ipc={fakeDiagnosticsIpc()} onOpen={onOpen} />);
    const capacity = await screen.findByTestId("diagnostics-entry-capacity", {}, WAIT);
    expect(capacity.getAttribute("data-level")).toBe("warn");
    fireEvent.click(screen.getByTestId("diagnostics-entry-open"));
    expect(onOpen).toHaveBeenCalledTimes(1);
  });
});

describe("BackupPage 备份提醒（DoD3）", () => {
  it("到期显示提醒；关闭调用 settings_set(false) 并刷新清单", async () => {
    const ipc = fakeBackupIpc();
    const settings = fakeSettingsIpc(true);
    render(<BackupPage ipc={ipc} settings={settings} onBack={() => {}} />);

    const reminder = await screen.findByTestId("backup-reminder", {}, WAIT);
    expect(reminder.getAttribute("data-reason")).toBe("never");
    fireEvent.click(screen.getByTestId("backup-reminder-dismiss"));

    await waitFor(() => {
      expect(settings.set).toHaveBeenCalledWith(BACKUP_REMINDER_KEY, false);
    }, WAIT);
    expect(ipc.list).toHaveBeenCalledTimes(2);
  });

  it("提醒关闭（enabled=false）时不渲染横幅", async () => {
    const ipc = fakeBackupIpc({
      list: vi.fn().mockResolvedValue({
        ...LIST_RESPONSE,
        reminder: { ...REMINDER_DUE, enabled: false, due: false },
      }),
    });
    render(<BackupPage ipc={ipc} settings={fakeSettingsIpc(true)} onBack={() => {}} />);
    await screen.findByTestId("capacity-status", {}, WAIT);
    expect(screen.queryByTestId("backup-reminder")).toBeNull();
  });
});

describe("App 覆盖层导航（M3-05 入口）", () => {
  it("顶栏入口可打开设置/诊断/关于覆盖层", async () => {
    const readySnapshot: StartupSnapshot = {
      phase: "ready",
      data_dir: "C:\\Local\\Aether",
      data_dir_source: "default",
      security_level: { level: "os", detail: "自检通过" },
    };
    vi.mocked(fetchStartup).mockResolvedValue(readySnapshot);
    render(<App />);
    await screen.findByTestId("app-version", {}, WAIT);

    fireEvent.click(screen.getByTestId("settings-open"));
    expect(await screen.findByTestId("settings-page", {}, WAIT)).toBeTruthy();
    expect(screen.getByTestId("settings-security-level").getAttribute("data-level")).toBe(
      "os",
    );

    fireEvent.click(screen.getByTestId("diagnostics-open"));
    await waitFor(() => {
      expect(screen.getByTestId("diagnostics-page")).toBeTruthy();
    }, WAIT);

    fireEvent.click(screen.getByTestId("about-open"));
    await waitFor(() => {
      expect(screen.getByTestId("about-page")).toBeTruthy();
    }, WAIT);
  });
});
