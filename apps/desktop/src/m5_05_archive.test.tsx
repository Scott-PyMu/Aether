/**
 * M5-05 前端集成测试：归档导出入口（一键归档锚点 `archive`、会话导出、结果回执）。
 *
 * 覆盖 DoD6 UI 面：
 * - 容量 2GB/5GB/增长三档联动（`archive` 的 `data-level` 与上下文文案）；
 * - 一键归档（缺省目录）与「归档到外部目录…」（选择器）两条入口；
 * - `backup_list.retention` 段渲染（保留开关/窗口/最近清理/失败计数/自动启停）；
 * - 单会话导出（`session-export`）→ `session_export` 命令 → `session-export-result`。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ArchiveIpc, ArchiveExportResponse } from "./archive";
import { BackupPage } from "./BackupPage";
import type { BackupIpc, BackupListResponse } from "./backup";
import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type { MessagesPageResult, SessionIpc, SessionSummary } from "./session";
import type { SettingsIpc } from "./settings";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";

const exportResponse: ArchiveExportResponse = {
  export: {
    path: "C:\\Local\\Aether\\archives\\aether-archive-1.jsonl",
    file_name: "aether-archive-1.jsonl",
    bytes: 4096,
    rows: 42,
    sha256: "a".repeat(64),
    source: "archive-export",
    session_id: null,
    created_at: 1_800_000_000_000,
    counts: { sessions: 1, messages: 2, events: 39, runs: 0, audits: 0 },
  },
};

function listResponse(level: "ok" | "warn" | "critical"): BackupListResponse {
  return {
    backups: [],
    capacity: {
      db_bytes: 1024,
      wal_bytes: 1024,
      total_bytes: 2048,
      warn_bytes: 2 * 1024 * 1024 * 1024,
      critical_bytes: 5 * 1024 * 1024 * 1024,
      level,
    },
    retention: {
      enabled: true,
      days: 30,
      cutoff_ts: 1_797_408_000_000,
      capacity: {
        level,
        total_bytes: 2048,
        warn_bytes: 2 * 1024 * 1024 * 1024,
        critical_bytes: 5 * 1024 * 1024 * 1024,
      },
      growth: { bytes_per_month: 640 * 1024 * 1024, threshold_bytes: 500 * 1024 * 1024, samples: 5 },
      auto_arm: { armed: true, at: 1_800_000_000_000, reason: "growth" },
      last_run_at: 1_800_000_000_000,
      last_result: "success",
      last_cleaned_rows: 123,
      last_freed_bytes: 4096,
      last_archive_failures: 0,
      archive_failures_total: 1,
      last_error: null,
    },
  };
}

function fakeBackupIpc(level: "ok" | "warn" | "critical" = "critical"): BackupIpc {
  return {
    list: vi.fn().mockResolvedValue(listResponse(level)),
    create: vi.fn(),
    restore: vi.fn(),
    pickTargetDir: vi.fn().mockResolvedValue(null),
  } as unknown as BackupIpc;
}

function fakeArchiveIpc(overrides: Partial<ArchiveIpc> = {}): ArchiveIpc {
  return {
    exportSession: vi.fn().mockResolvedValue({
      export: {
        ...exportResponse.export,
        file_name: `session-${SESSION}-1.jsonl`,
        session_id: SESSION,
        source: "session-export",
      },
    }),
    exportAll: vi.fn().mockResolvedValue(exportResponse),
    pickTargetDir: vi.fn().mockResolvedValue("E:\\archives"),
    ...overrides,
  };
}

const settingsStub: SettingsIpc = {
  get: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
  set: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const stores: EventStore[] = [];

afterEach(() => {
  for (const store of stores.splice(0)) {
    store.dispose();
  }
});

describe("BackupPage 归档入口（M5-05）", () => {
  it("一键归档：临界档联动（data-level=critical）+ 缺省目录 + data-result=ok", async () => {
    const ipc = fakeBackupIpc("critical");
    const archive = fakeArchiveIpc();
    render(
      <BackupPage ipc={ipc} settings={settingsStub} archive={archive} onBack={() => {}} />,
    );

    const button = await screen.findByTestId("archive");
    expect(button.getAttribute("data-level")).toBe("critical");
    expect(button.textContent).toContain("强烈建议");
    fireEvent.click(button);
    await waitFor(() => {
      expect(archive.exportAll).toHaveBeenCalledWith();
    });
    const result = await screen.findByTestId("archive-export-result");
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain("aether-archive-1.jsonl");
    expect(screen.getByTestId("archive").getAttribute("data-result")).toBe("ok");
  });

  it("归档到外部目录：选择器返回目录后透传 target_dir；取消时提示不改数据", async () => {
    const ipc = fakeBackupIpc("warn");
    const cancelled = fakeArchiveIpc({ pickTargetDir: vi.fn().mockResolvedValue(null) });
    const { unmount } = render(
      <BackupPage ipc={ipc} settings={settingsStub} archive={cancelled} onBack={() => {}} />,
    );
    fireEvent.click(await screen.findByTestId("archive-external"));
    await waitFor(() => {
      expect(cancelled.pickTargetDir).toHaveBeenCalled();
    });
    expect(cancelled.exportAll).not.toHaveBeenCalled();
    expect(screen.getByTestId("archive-export-result").getAttribute("data-result")).toBe(
      "failed",
    );
    unmount();

    const archive = fakeArchiveIpc();
    render(<BackupPage ipc={ipc} settings={settingsStub} archive={archive} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("archive-external"));
    await waitFor(() => {
      expect(archive.exportAll).toHaveBeenCalledWith("E:\\archives");
    });
  });

  it("保留状态段：开关/窗口/最近清理/失败计数/自动启停渲染（retention-status）", async () => {
    const ipc = fakeBackupIpc("ok");
    render(
      <BackupPage ipc={ipc} settings={settingsStub} archive={fakeArchiveIpc()} onBack={() => {}} />,
    );
    const status = await screen.findByTestId("retention-status");
    expect(status.getAttribute("data-enabled")).toBe("true");
    expect(status.getAttribute("data-auto-armed")).toBe("true");
    expect(status.textContent).toContain("保留窗口 30 天");
    expect(status.textContent).toContain("123 条");
    expect(status.textContent).toContain("归档失败累计 1 次");
    expect(status.textContent).toContain("事件增长 >500MB/月");
    // ok 档：一键归档仍可用（用户主动归档），但无警告文案。
    expect(screen.getByTestId("archive").getAttribute("data-level")).toBe("ok");
  });

  it("一键归档失败：结构化错误码渲染（data-result=failed）", async () => {
    const ipc = fakeBackupIpc("critical");
    const archive = fakeArchiveIpc({
      exportAll: vi.fn().mockRejectedValue({
        code: "invalid_value",
        message: "目标可用空间不足（archive_space_insufficient）",
      }),
    });
    render(<BackupPage ipc={ipc} settings={settingsStub} archive={archive} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("archive"));
    const result = await screen.findByTestId("archive-export-result");
    expect(result.getAttribute("data-result")).toBe("failed");
    expect(result.textContent).toContain("archive_space_insufficient");
  });
});

describe("SessionWorkbench 会话导出（M5-05）", () => {
  it("session-export：透传会话 id → 结果回执（data-result=ok）", async () => {
    const session: SessionSummary = {
      id: SESSION,
      runtime_id: "mock",
      workspace_id: null,
      parent_session_id: null,
      title: "导出会话",
      status: "idle",
      model: null,
      created_at: 1,
      updated_at: 1,
      closed_at: null,
    };
    const page: MessagesPageResult = {
      session_id: SESSION,
      max_seq: null,
      events: [],
      messages: [],
      complete: true,
    };
    const ipc: SessionIpc = {
      listRuntimes: vi.fn().mockResolvedValue([]),
      listSessions: vi.fn().mockResolvedValue([session]),
      createSession: vi.fn(),
      sendMessage: vi.fn(),
      interruptSession: vi.fn(),
      disposeSession: vi.fn(),
      messagesPage: vi.fn().mockResolvedValue(page),
      retryRun: vi.fn(),
    } as unknown as SessionIpc;
    const archive = fakeArchiveIpc();
    const store = new EventStore();
    stores.push(store);
    render(<SessionWorkbench store={store} ipc={ipc} archiveIpc={archive} />);

    fireEvent.click(await screen.findByTestId("session-item"));
    const exportButton = await screen.findByTestId("session-export");
    fireEvent.click(exportButton);
    await waitFor(() => {
      expect(archive.exportSession).toHaveBeenCalledWith(SESSION);
    });
    const result = await screen.findByTestId("session-export-result");
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain("已导出");
    expect(screen.getByTestId("session-export").getAttribute("data-result")).toBe("ok");
  });
});
