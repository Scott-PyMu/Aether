/**
 * M6-03 前端集成测试：审计页（锚点 `audit-page`/`audit-filter`/`audit-export`）。
 *
 * 覆盖 DoD：
 * - DoD2 查询：过滤（actor）与分页（offset）经 `audit_query` 透传；行渲染与总数；
 * - DoD3 导出：外部目录经选择器 → `audit_export` 透传过滤 + target_dir；回执
 *   `audit-export-result`（ok/failed）与结构化错误码（`audit-error` data-code）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AuditPage, dateTimeToMs } from "./AuditPage";
import type { AuditIpc, AuditRow } from "./audit";

function row(id: string, overrides: Partial<AuditRow> = {}): AuditRow {
  return {
    id,
    session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
    runtime_id: "mock",
    actor: "agent",
    action: "tool.call",
    resource: "mock.echo",
    detail: '{"tool_call_id":"c1","duration_ms":12}',
    result: "success",
    ts: 1_800_000_000_000,
    ...overrides,
  };
}

function fakeAuditIpc(overrides: Partial<AuditIpc> = {}): AuditIpc {
  return {
    query: vi.fn().mockResolvedValue({
      rows: [row("a1"), row("a2", { result: "failed", resource: "mock.read_file" })],
      limit: 100,
      offset: 0,
      has_more: false,
    }),
    exportAudit: vi.fn().mockResolvedValue({
      export: {
        path: "C:\\Local\\Aether\\archives\\aether-audit-1.jsonl",
        file_name: "aether-audit-1.jsonl",
        bytes: 2048,
        rows: 2,
        sha256: "b".repeat(64),
        source: "audit-export",
        session_id: null,
        created_at: 1_800_000_000_000,
        counts: { sessions: 0, messages: 0, events: 0, runs: 0, audits: 2 },
      },
    }),
    pickTargetDir: vi.fn().mockResolvedValue("E:\\audit-out"),
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("AuditPage（M6-03）", () => {
  it("挂载即查询：锚点齐备、行渲染、总数与分页按钮状态", async () => {
    const ipc = fakeAuditIpc();
    render(<AuditPage ipc={ipc} onBack={() => {}} />);

    expect(screen.getByTestId("audit-page")).toBeTruthy();
    expect(screen.getByTestId("audit-filter")).toBeTruthy();
    expect(screen.getByTestId("audit-export")).toBeTruthy();

    await waitFor(() => {
      expect(ipc.query).toHaveBeenCalledWith(
        {
          sessionId: undefined,
          actor: undefined,
          action: undefined,
          resource: undefined,
          fromTs: undefined,
          toTs: undefined,
        },
        { limit: 100, offset: 0 },
      );
    });
    const rows = await screen.findAllByTestId("audit-row");
    expect(rows).toHaveLength(2);
    const first = rows.at(0);
    expect(first?.getAttribute("data-row-id")).toBe("a1");
    expect(screen.getByTestId("audit-summary").textContent).toContain("已到末页");
    expect(screen.getByTestId("audit-prev")).toHaveProperty("disabled", true);
    expect(screen.getByTestId("audit-next")).toHaveProperty("disabled", true);
  });

  it("过滤 + 分页：actor 透传与下一页 offset", async () => {
    const query = vi
      .fn()
      .mockResolvedValueOnce({ rows: [row("a1")], limit: 100, offset: 0, has_more: true })
      .mockResolvedValueOnce({ rows: [row("a1")], limit: 100, offset: 0, has_more: true })
      .mockResolvedValueOnce({ rows: [row("b1")], limit: 100, offset: 100, has_more: false });
    const ipc = fakeAuditIpc({ query });
    render(<AuditPage ipc={ipc} onBack={() => {}} />);
    await screen.findAllByTestId("audit-row");

    fireEvent.change(screen.getByTestId("audit-filter-actor"), {
      target: { value: "agent" },
    });
    fireEvent.click(screen.getByTestId("audit-query"));
    await waitFor(() => {
      expect(query).toHaveBeenLastCalledWith(
        expect.objectContaining({ actor: "agent" }),
        { limit: 100, offset: 0 },
      );
    });

    fireEvent.click(screen.getByTestId("audit-next"));
    await waitFor(() => {
      expect(query).toHaveBeenLastCalledWith(expect.anything(), { limit: 100, offset: 100 });
    });
    expect(screen.getByTestId("audit-prev")).toHaveProperty("disabled", false);
  });

  it("导出：选择外部目录后透传 target_dir；回执 data-result=ok", async () => {
    const ipc = fakeAuditIpc();
    render(<AuditPage ipc={ipc} onBack={() => {}} />);
    await screen.findAllByTestId("audit-row");

    fireEvent.change(screen.getByTestId("audit-filter-action"), {
      target: { value: "tool.call" },
    });
    fireEvent.click(screen.getByTestId("audit-pick"));
    await waitFor(() => {
      expect(ipc.pickTargetDir).toHaveBeenCalled();
    });
    fireEvent.click(screen.getByTestId("audit-export"));
    await waitFor(() => {
      expect(ipc.exportAudit).toHaveBeenCalledWith(
        expect.objectContaining({ action: "tool.call" }),
        "E:\\audit-out",
      );
    });
    const result = await screen.findByTestId("audit-export-result");
    expect(result.getAttribute("data-result")).toBe("ok");
    expect(result.textContent).toContain("aether-audit-1.jsonl");
  });

  it("导出失败：结构化错误码渲染（data-result=failed + data-code）", async () => {
    const ipc = fakeAuditIpc({
      exportAudit: vi.fn().mockRejectedValue({
        code: "invalid_value",
        message: "目标可用空间不足（archive_space_insufficient）",
      }),
    });
    render(<AuditPage ipc={ipc} onBack={() => {}} />);
    await screen.findAllByTestId("audit-row");

    fireEvent.click(screen.getByTestId("audit-export"));
    const result = await screen.findByTestId("audit-export-result");
    expect(result.getAttribute("data-result")).toBe("failed");
    const error = await screen.findByTestId("audit-error");
    expect(error.getAttribute("data-code")).toBe("invalid_value");
    expect(error.textContent).toContain("archive_space_insufficient");
  });

  it("datetime-local 转换：非法值不参与过滤（from 缺省）", () => {
    expect(dateTimeToMs("")).toBeUndefined();
    expect(dateTimeToMs("not-a-date")).toBeUndefined();
    expect(typeof dateTimeToMs("2026-10-14T10:30")).toBe("number");
  });
});
