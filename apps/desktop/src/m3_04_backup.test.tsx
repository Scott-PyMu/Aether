/**
 * M3-04 前端集成测试：备份与恢复页（S-06/§7.3 锚点；DoD5/DoD6 的 UI 面）。
 *
 * 覆盖：清单/容量渲染、创建（内部/外部选择器/空间不足提示）、恢复确认流
 * （内部/外部 → `restart_required` 展示；失败 data-result=failed）、覆盖层返回。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App } from "./App";
import { BackupPage } from "./BackupPage";
import type { BackupIpc, BackupListResponse } from "./backup";
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

const listResponse: BackupListResponse = {
  backups: [
    {
      id: "01J000000000000000000000B1",
      path: "C:\\Local\\Aether\\backups\\aether-1.db",
      size_bytes: 2048,
      encrypted: false,
      kind: "internal",
      created_at: 1_700_000_000_000,
    },
    {
      id: "01J000000000000000000000B2",
      path: "E:\\external\\aether-2.db",
      size_bytes: 4096,
      encrypted: false,
      kind: "external",
      created_at: 1_700_000_001_000,
    },
  ],
  capacity: {
    db_bytes: 1024,
    wal_bytes: 1024,
    total_bytes: 2048,
    warn_bytes: 2 * 1024 * 1024 * 1024,
    critical_bytes: 5 * 1024 * 1024 * 1024,
    level: "warn",
  },
};

function fakeIpc(overrides: Partial<BackupIpc> = {}): BackupIpc {
  const firstBackup = listResponse.backups[0]!;
  return {
    list: vi.fn().mockResolvedValue(listResponse),
    create: vi.fn().mockResolvedValue({
      backup: firstBackup,
      pruned: [],
    }),
    restore: vi.fn().mockResolvedValue({
      restoring: true,
      restart_required: true,
      source: "internal",
      candidate: {
        path: firstBackup.path,
        size_bytes: 2048,
        schema_version: 2,
      },
      db_path: "C:\\Local\\Aether\\aether.db",
    }),
    pickTargetDir: vi.fn().mockResolvedValue(null),
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("BackupPage", () => {
  it("渲染备份清单（data-kind）与容量状态（data-level）", async () => {
    const ipc = fakeIpc();
    render(<BackupPage ipc={ipc} onBack={() => {}} />);

    const items = await screen.findAllByTestId("backup-item");
    expect(items).toHaveLength(2);
    const internal = items[0]!;
    const external = items[1]!;
    expect(internal.getAttribute("data-kind")).toBe("internal");
    expect(external.getAttribute("data-kind")).toBe("external");
    expect(internal.getAttribute("data-backup-id")).toBe(
      listResponse.backups[0]!.id,
    );

    const capacity = screen.getByTestId("capacity-status");
    expect(capacity.getAttribute("data-level")).toBe("warn");
    expect(capacity.textContent).toContain("容量警告");
    expect(capacity.textContent).toContain("2.0 KiB");
  });

  it("创建内部备份：标签透传、完成后刷新清单", async () => {
    const ipc = fakeIpc();
    render(<BackupPage ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("backup-list");

    fireEvent.change(screen.getByTestId("backup-create-label"), {
      target: { value: "发布前" },
    });
    fireEvent.click(screen.getByTestId("backup-create"));

    await waitFor(() => {
      expect(ipc.create).toHaveBeenCalledWith("发布前", null);
    });
    expect(await screen.findByTestId("backup-notice")).toBeTruthy();
    expect(ipc.list).toHaveBeenCalledTimes(2);
  });

  it("创建外部备份：系统选择器返回目录 → target_dir 透传；取消不调用创建", async () => {
    const create = vi.fn().mockResolvedValue({
      backup: { ...listResponse.backups[1] },
      pruned: [],
    });
    const ipc = fakeIpc({
      create,
      pickTargetDir: vi.fn().mockResolvedValue("E:\\external"),
    });
    render(<BackupPage ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("backup-list");

    fireEvent.click(screen.getByTestId("backup-create-external"));
    await waitFor(() => {
      expect(create).toHaveBeenCalledWith(null, "E:\\external");
    });

    create.mockClear();
    const cancelIpc = fakeIpc({ pickTargetDir: vi.fn().mockResolvedValue(null) });
    cleanup();
    render(<BackupPage ipc={cancelIpc} onBack={() => {}} />);
    await screen.findByTestId("backup-list");
    fireEvent.click(screen.getByTestId("backup-create-external"));
    await screen.findByTestId("backup-notice");
    expect(cancelIpc.create).not.toHaveBeenCalled();
  });

  it("空间不足：错误码 invalid_value + 稳定业务码提示（ADR-003 决策 19）", async () => {
    const ipc = fakeIpc({
      create: vi.fn().mockRejectedValue({
        code: "invalid_value",
        message:
          "目标可用空间不足（backup_space_insufficient）：可用 10 字节 < 需求 120 字节",
      }),
    });
    render(<BackupPage ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("backup-list");

    fireEvent.click(screen.getByTestId("backup-create"));
    const error = await screen.findByTestId("backup-error");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("backup_space_insufficient");
  });

  it("恢复内部备份：二次确认 → internal 来源 → requested 结果；返回按钮回调", async () => {
    const onBack = vi.fn();
    const ipc = fakeIpc();
    render(<BackupPage ipc={ipc} onBack={onBack} />);
    const items = await screen.findAllByTestId("backup-item");

    fireEvent.click(
      items[0]!.querySelector('[data-testid="backup-restore"]') as HTMLElement,
    );
    const confirm = await screen.findByTestId("backup-restore-confirm");
    expect(confirm.getAttribute("data-source")).toBe("internal");
    fireEvent.click(confirm);

    await waitFor(() => {
      expect(ipc.restore).toHaveBeenCalledWith({
        internal: { id: listResponse.backups[0]!.id },
      });
    });
    const result = await screen.findByTestId("backup-restore-result");
    expect(result.getAttribute("data-result")).toBe("requested");
    expect(result.getAttribute("data-source")).toBe("internal");
    expect(result.textContent).toContain("重启");

    fireEvent.click(screen.getByTestId("overlay-back"));
    expect(onBack).toHaveBeenCalledTimes(1);
  });

  it("恢复外部候选：路径输入 → external 来源；失败展示 data-result=failed", async () => {
    const restore = vi
      .fn()
      .mockRejectedValue({ code: "invalid_value", message: "候选校验失败（backup_candidate_invalid）" });
    const ipc = fakeIpc({ restore });
    render(<BackupPage ipc={ipc} onBack={() => {}} />);
    await screen.findByTestId("backup-list");

    fireEvent.change(screen.getByTestId("backup-restore-external"), {
      target: { value: "E:\\backup\\aether.db" },
    });
    fireEvent.click(screen.getByTestId("backup-restore-external-submit"));
    const confirm = await screen.findByTestId("backup-restore-confirm");
    expect(confirm.getAttribute("data-source")).toBe("external");
    fireEvent.click(confirm);

    await waitFor(() => {
      expect(restore).toHaveBeenCalledWith({
        external: { path: "E:\\backup\\aether.db" },
      });
    });
    const result = await screen.findByTestId("backup-restore-result");
    expect(result.getAttribute("data-result")).toBe("failed");
    expect(result.getAttribute("data-source")).toBe("external");
    expect(screen.getByTestId("backup-error").getAttribute("data-code")).toBe(
      "invalid_value",
    );
  });
});

describe("App 覆盖层入口", () => {
  const readySnapshot: StartupSnapshot = {
    phase: "ready",
    data_dir: "C:\\Local\\Aether",
    data_dir_source: "default",
  };

  it("备份入口打开覆盖层并返回工作台（工作台不卸载）", async () => {
    vi.mocked(fetchStartup).mockResolvedValue(readySnapshot);
    render(<App />);
    await screen.findByTestId("app-version");

    expect(screen.queryByTestId("overlay-backup")).toBeNull();
    fireEvent.click(screen.getByTestId("backup-open"));
    expect(await screen.findByTestId("overlay-backup")).toBeTruthy();
    // 工作台仍在 DOM（覆盖层语义：不卸载）。
    expect(screen.getByTestId("app-data-dir")).toBeTruthy();

    fireEvent.click(screen.getByTestId("overlay-back"));
    await waitFor(() => {
      expect(screen.queryByTestId("overlay-backup")).toBeNull();
    });
  });
});
