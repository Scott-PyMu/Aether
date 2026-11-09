/**
 * M10-01/ADR-031：工作流列表 / JSON 编辑校验 / 运行视图 / 节点重试（UI 最小集）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { WorkflowsPage } from "./WorkflowsPage";
import type {
  WorkflowDetail,
  WorkflowIpc,
  WorkflowRunDetail,
  WorkflowSummary,
} from "./workflows";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const WORKFLOW = "01J8ZQ5R0N7W9Y8X6V4T2S0K1W";
const RUN = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";

const DEFINITION = {
  schema_version: 1,
  name: "demo",
  nodes: [{ id: "start", type: "task", handler: "noop" }],
  edges: [],
};

const SUMMARY: WorkflowSummary = {
  workflow_id: WORKFLOW,
  name: "demo",
  description: null,
  version: 1,
  is_template: false,
  created_at: 1,
  updated_at: 1,
};

const DETAIL: WorkflowDetail = { ...SUMMARY, definition: DEFINITION };

function fakeIpc(overrides: Partial<WorkflowIpc> = {}): WorkflowIpc {
  const runDetail: WorkflowRunDetail = {
    workflow_run_id: RUN,
    workflow_id: WORKFLOW,
    session_id: SESSION,
    status: "succeeded",
    priority: 0,
    started_at: 10,
    finished_at: 20,
    error: null,
    nodes: [
      {
        node_run_id: "n1",
        node_id: "start",
        type: "task",
        status: "failed",
        attempt: 1,
        started_at: 10,
        finished_at: 15,
        error: "injected",
        result_summary: null,
      },
    ],
  };
  return {
    list: vi.fn(async () => ({ items: [SUMMARY], total: 1 })),
    get: vi.fn(async () => DETAIL),
    create: vi.fn(async () => SUMMARY),
    update: vi.fn(async () => DETAIL),
    remove: vi.fn(async () => ({ deleted: true })),
    importWorkflow: vi.fn(async () => DETAIL),
    run: vi.fn(async () => ({ workflow_run_id: RUN, status: "queued" })),
    runGet: vi.fn(async () => runDetail),
    runList: vi.fn(async () => ({ items: [], total: 0 })),
    cancel: vi.fn(async () => ({ status: "cancelled" })),
    retryNode: vi.fn(async () => ({ node_run_id: "n2", attempt: 2 })),
    ...overrides,
  };
}

afterEach(cleanup);

describe("M10-01 workflow UI", () => {
  it("lists workflows, edits/validates JSON and renders run/node status", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);

    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    fireEvent.click(screen.getByTestId("workflow-item"));
    await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));

    const editor = screen.getByTestId("workflow-editor") as HTMLTextAreaElement;
    expect(editor.value).toContain("\"schema_version\"");

    const validate = screen.getByTestId("workflow-editor-validate");
    fireEvent.click(validate);
    expect(validate.getAttribute("data-valid")).toBe("true");

    fireEvent.change(editor, { target: { value: "{not-json" } });
    fireEvent.click(validate);
    expect(validate.getAttribute("data-valid")).toBe("false");
    expect(validate.getAttribute("data-error-code")).toBe("invalid_json");

    fireEvent.change(editor, { target: { value: JSON.stringify(DEFINITION) } });
    fireEvent.click(screen.getByText("运行"));
    await waitFor(() =>
      expect(screen.getByTestId("workflow-run-view").getAttribute("data-run-id")).toBe(RUN),
    );
    expect(screen.getByTestId("workflow-run-view").getAttribute("data-status")).toBe("succeeded");
    const node = screen.getByTestId("workflow-node-status");
    expect(node.getAttribute("data-node-id")).toBe("start");
    expect(node.getAttribute("data-attempt")).toBe("1");

    fireEvent.click(screen.getByTestId("workflow-node-retry"));
    await waitFor(() =>
      expect(ipc.retryNode).toHaveBeenCalledWith(RUN, "start", true),
    );
  });

  it("imports a JSON definition path", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    const input = screen.getByPlaceholderText("E:\\path\\workflow.json") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "E:\\tmp\\wf.json" } });
    await waitFor(() => expect(input.value).toBe("E:\\tmp\\wf.json"));
    fireEvent.click(screen.getByTestId("workflow-import"));
    await waitFor(() => expect(ipc.importWorkflow).toHaveBeenCalledWith("E:\\tmp\\wf.json"));
  });
});
