/**
 * M10-04 前端集成测试：S-06 导入入口/进度/警告/取消/失败恢复（ADR-034）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { BackupIpc, BackupListResponse } from "./backup";
import { BackupPage } from "./BackupPage";
import type { ImportIpc, ImportJobStatus } from "./import";
import type { SettingsIpc } from "./settings";

function listResponse(): BackupListResponse {
  return {
    backups: [],
    capacity: {
      db_bytes: 1024,
      wal_bytes: 1024,
      total_bytes: 2048,
      warn_bytes: 2 * 1024 * 1024 * 1024,
      critical_bytes: 5 * 1024 * 1024 * 1024,
      level: "ok",
    },
    retention: null,
    auto: null,
  } as unknown as BackupListResponse;
}

function fakeBackupIpc(): BackupIpc {
  return {
    list: vi.fn().mockResolvedValue(listResponse()),
    create: vi.fn(),
    restore: vi.fn(),
    pickTargetDir: vi.fn().mockResolvedValue(null),
  } as unknown as BackupIpc;
}

const settingsStub: SettingsIpc = {
  get: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
  set: vi.fn().mockResolvedValue({ key: "backup.reminder", value: true }),
};

function job(status: ImportJobStatus["status"], overrides: Partial<ImportJobStatus> = {}): ImportJobStatus {
  return {
    job_id: "01J00000000000000000000J10",
    kind: "session",
    status,
    mode: "remap",
    dry_run: false,
    source_path: "E:\\exports\\session.jsonl",
    source_bytes: 4096,
    progress: { rows: 3, bytes: 4096, total_bytes: 4096 },
    summary: {
      sessions: 1,
      messages: 2,
      events: 1,
      runs: 0,
      audits: 0,
      duplicates: 0,
      skipped: 0,
    },
    warnings: [
      { code: "workspace_missing", path: "line:2", detail: "工作区不存在；按无工作区导入" },
    ],
    warnings_truncated: false,
    error: null,
    created_at: 1,
    updated_at: 1,
    ...overrides,
  };
}

function fakeImport(overrides: Partial<ImportIpc> = {}): ImportIpc {
  return {
    startSession: vi.fn().mockResolvedValue({ job_id: "01J00000000000000000000J10", status: "staging" }),
    startArchive: vi.fn().mockResolvedValue({ job_id: "01J00000000000000000000J11", status: "staging" }),
    status: vi.fn().mockResolvedValue(job("awaiting_restart")),
    cancel: vi.fn().mockResolvedValue({ status: "cancelled" }),
    pickFile: vi.fn().mockResolvedValue("E:\\exports\\session.jsonl"),
    restart: vi.fn().mockResolvedValue(undefined),
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("BackupPage 导入（M10-04）", () => {
  it("选择文件 → 导入会话 → awaiting_restart（警告/汇总/重启入口）", async () => {
    const importer = fakeImport();
    render(
      <BackupPage ipc={fakeBackupIpc()} settings={settingsStub} importer={importer} onBack={() => {}} />,
    );
    fireEvent.click(await screen.findByTestId("import-pick"));
    await waitFor(() => {
      expect(importer.pickFile).toHaveBeenCalled();
    });
    fireEvent.click(screen.getByTestId("import"));
    await waitFor(() => {
      expect(importer.startSession).toHaveBeenCalledWith(
        "E:\\exports\\session.jsonl",
        "remap",
        false,
      );
    });
    const status = await screen.findByTestId("import-status");
    expect(status.getAttribute("data-status")).toBe("awaiting_restart");
    expect(screen.getByTestId("import-summary").textContent).toContain("会话 1");
    expect(screen.getByTestId("import-warnings").textContent).toContain("workspace_missing");
    fireEvent.click(screen.getByTestId("import-restart"));
    await waitFor(() => {
      expect(importer.restart).toHaveBeenCalled();
    });
  });

  it("dry-run 完成不出现重启入口", async () => {
    const importer = fakeImport({
      startSession: vi
        .fn()
        .mockResolvedValue({ job_id: "01J00000000000000000000J12", status: "staging" }),
      status: vi.fn().mockResolvedValue(job("completed", { dry_run: true })),
    });
    render(
      <BackupPage ipc={fakeBackupIpc()} settings={settingsStub} importer={importer} onBack={() => {}} />,
    );
    fireEvent.change(await screen.findByTestId("import-file"), {
      target: { value: "E:\\exports\\session.jsonl" },
    });
    fireEvent.click(screen.getByTestId("import-dry-run"));
    await waitFor(() => {
      expect(importer.startSession).toHaveBeenCalledWith(
        "E:\\exports\\session.jsonl",
        "remap",
        true,
      );
    });
    const status = await screen.findByTestId("import-status");
    expect(status.getAttribute("data-status")).toBe("completed");
    expect(status.getAttribute("data-dry-run")).toBe("true");
    expect(screen.queryByTestId("import-restart")).toBeNull();
  });

  it("取消活动作业：cancel → cancelled", async () => {
    const importer = fakeImport({
      status: vi
        .fn()
        .mockResolvedValueOnce(job("validating"))
        .mockResolvedValue(job("cancelled", { summary: null, warnings: [] })),
    });
    render(
      <BackupPage ipc={fakeBackupIpc()} settings={settingsStub} importer={importer} onBack={() => {}} />,
    );
    fireEvent.change(await screen.findByTestId("import-file"), {
      target: { value: "E:\\exports\\session.jsonl" },
    });
    fireEvent.click(screen.getByTestId("import"));
    await screen.findByTestId("import-cancel");
    fireEvent.click(screen.getByTestId("import-cancel"));
    await waitFor(() => {
      expect(importer.cancel).toHaveBeenCalledWith("01J00000000000000000000J10");
    });
    await waitFor(() => {
      expect(screen.getByTestId("import-status").getAttribute("data-status")).toBe("cancelled");
    });
  });

  it("提交失败：结构化业务码渲染（import_invalid_format）", async () => {
    const importer = fakeImport({
      startSession: vi.fn().mockRejectedValue({
        code: "invalid_format",
        message: "导入文件格式无效",
        business_code: "import_invalid_format",
      }),
    });
    render(
      <BackupPage ipc={fakeBackupIpc()} settings={settingsStub} importer={importer} onBack={() => {}} />,
    );
    fireEvent.change(await screen.findByTestId("import-file"), {
      target: { value: "E:\\exports\\bad.jsonl" },
    });
    fireEvent.click(screen.getByTestId("import"));
    const error = await screen.findByTestId("import-submit-error");
    expect(error.textContent).toContain("导入文件格式无效");
  });

  it("归档导入按钮透传 archive 命令", async () => {
    const importer = fakeImport();
    render(
      <BackupPage ipc={fakeBackupIpc()} settings={settingsStub} importer={importer} onBack={() => {}} />,
    );
    fireEvent.change(await screen.findByTestId("import-file"), {
      target: { value: "E:\\exports\\archive.jsonl" },
    });
    fireEvent.click(screen.getByTestId("import-archive"));
    await waitFor(() => {
      expect(importer.startArchive).toHaveBeenCalledWith(
        "E:\\exports\\archive.jsonl",
        "remap",
        false,
      );
    });
  });
});
