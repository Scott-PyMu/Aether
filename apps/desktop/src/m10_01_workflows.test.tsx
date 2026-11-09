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

  it("rejects null/array/schema/nodes/edges definitions with stable codes", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    const editor = screen.getByTestId("workflow-editor") as HTMLTextAreaElement;
    const validate = screen.getByTestId("workflow-editor-validate");

    const cases: Array<[string, string]> = [
      ["null", "invalid_shape"],
      ["[]", "invalid_shape"],
      [JSON.stringify({ schema_version: 2, nodes: [{}], edges: [] }), "schema_version"],
      [JSON.stringify({ schema_version: 1, nodes: [], edges: [] }), "invalid_value"],
      [JSON.stringify({ schema_version: 1, nodes: [{}] }), "invalid_value"],
    ];
    for (const [text, code] of cases) {
      fireEvent.change(editor, { target: { value: text } });
      fireEvent.click(validate);
      expect(validate.getAttribute("data-error-code")).toBe(code);
    }
  });

  it("creates, updates and surfaces save failures", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());

    fireEvent.click(screen.getByText("创建"));
    await waitFor(() => expect(ipc.create).toHaveBeenCalled());
    expect(ipc.get).toHaveBeenCalledWith(WORKFLOW);
    expect(screen.getByText("保存新版本")).toBeTruthy();

    fireEvent.click(screen.getByText("保存新版本"));
    await waitFor(() =>
      expect(ipc.update).toHaveBeenCalledWith(
        expect.objectContaining({
          workflow_id: WORKFLOW,
          expected_version: 1,
        }),
      ),
    );

    vi.mocked(ipc.update).mockRejectedValueOnce(new Error("update failed"));
    fireEvent.click(screen.getByText("保存新版本"));
    expect((await screen.findByTestId("workflow-error")).textContent).toContain(
      "update failed",
    );
  });

  it("rejects empty import paths and surfaces import failures", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());

    fireEvent.click(screen.getByTestId("workflow-import"));
    expect((await screen.findByRole("alert")).textContent).toContain("请填写导入路径");

    const input = screen.getByPlaceholderText("E:\\path\\workflow.json") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "E:\\tmp\\missing.json" } });
    vi.mocked(ipc.importWorkflow).mockRejectedValueOnce(new Error("import failed"));
    fireEvent.click(screen.getByTestId("workflow-import"));
    expect((await screen.findByTestId("workflow-error")).textContent).toContain(
      "import failed",
    );
  });

  it("validates run input, starts/cancels a running workflow and polls", async () => {
    const running: WorkflowRunDetail = {
      workflow_run_id: RUN,
      workflow_id: WORKFLOW,
      session_id: SESSION,
      status: "running",
      priority: 0,
      started_at: 10,
      finished_at: null,
      error: null,
      nodes: [
        {
          node_run_id: "n1",
          node_id: "start",
          type: "task",
          status: "running",
          attempt: 1,
          started_at: 10,
          finished_at: null,
          error: null,
          result_summary: null,
        },
      ],
    };
    const succeeded: WorkflowRunDetail = {
      ...running,
      status: "succeeded",
      finished_at: 20,
      nodes: [],
    };
    const ipc = fakeIpc({
      runGet: vi
        .fn()
        .mockResolvedValueOnce(running)
        .mockResolvedValue(succeeded),
    });
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    fireEvent.click(screen.getByTestId("workflow-item"));
    await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));

    const runInput = screen.getByDisplayValue("{}") as HTMLTextAreaElement;
    fireEvent.change(runInput, { target: { value: "{not-json" } });
    fireEvent.click(screen.getByText("运行"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "运行输入不是合法 JSON",
    );

    fireEvent.change(runInput, { target: { value: "{}" } });
    fireEvent.click(screen.getByText("运行"));
    await waitFor(() =>
      expect(screen.getByTestId("workflow-run-view").getAttribute("data-status")).toBe(
        "running",
      ),
    );
    await waitFor(() => expect(ipc.runGet).toHaveBeenCalledTimes(2), {
      timeout: 2500,
    });
    await waitFor(() =>
      expect(screen.getByTestId("workflow-run-view").getAttribute("data-status")).toBe(
        "succeeded",
      ),
    );
  });

  it("surfaces list/select/retry failures", async () => {
    const listIpc = fakeIpc({
      list: vi.fn(async () => {
        throw new Error("list failed");
      }),
    });
    const { unmount } = render(
      <WorkflowsPage ipc={listIpc} onBack={() => {}} sessionId={SESSION} />,
    );
    expect((await screen.findByTestId("workflow-error")).textContent).toContain(
      "list failed",
    );
    unmount();

    const selectIpc = fakeIpc({
      get: vi.fn(async () => {
        throw new Error("get failed");
      }),
    });
    const second = render(
      <WorkflowsPage ipc={selectIpc} onBack={() => {}} sessionId={SESSION} />,
    );
    fireEvent.click(await screen.findByTestId("workflow-item"));
    expect((await screen.findByTestId("workflow-error")).textContent).toContain(
      "get failed",
    );
    second.unmount();
  });

  it("requires anchor session and accepts empty run input as null", async () => {
    const ipc = fakeIpc();
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    fireEvent.click(screen.getByTestId("workflow-item"));
    await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));

    const sessionInput = screen.getByPlaceholderText("26 位 ULID") as HTMLInputElement;
    fireEvent.change(sessionInput, { target: { value: "" } });
    fireEvent.click(screen.getByText("运行"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "请填写锚点会话 id",
    );

    fireEvent.change(sessionInput, { target: { value: SESSION } });
    const runInput = screen.getByDisplayValue("{}") as HTMLTextAreaElement;
    fireEvent.change(runInput, { target: { value: "" } });
    fireEvent.click(screen.getByText("运行"));
    await waitFor(() =>
      expect(ipc.run).toHaveBeenCalledWith(
        expect.objectContaining({ workflow_id: WORKFLOW, input: null }),
      ),
    );
  });

  it("cancels a running workflow and refreshes its state", async () => {
    const running: WorkflowRunDetail = {
      workflow_run_id: RUN,
      workflow_id: WORKFLOW,
      session_id: SESSION,
      status: "running",
      priority: 0,
      started_at: 10,
      finished_at: null,
      error: null,
      nodes: [],
    };
    const cancelled: WorkflowRunDetail = {
      ...running,
      status: "cancelled",
      finished_at: 20,
    };
    const ipc = fakeIpc({
      runGet: vi
        .fn()
        .mockResolvedValueOnce(running)
        .mockResolvedValue(cancelled),
    });
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    fireEvent.click(screen.getByTestId("workflow-item"));
    await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));
    fireEvent.click(screen.getByText("运行"));
    await waitFor(() =>
      expect(screen.getByTestId("workflow-run-view").getAttribute("data-status")).toBe(
        "running",
      ),
    );

    fireEvent.click(screen.getByTestId("workflow-run-cancel"));
    await waitFor(() =>
      expect(ipc.cancel).toHaveBeenCalledWith(RUN, "user"),
    );
    await waitFor(() =>
      expect(screen.getByTestId("workflow-run-view").getAttribute("data-status")).toBe(
        "cancelled",
      ),
    );
  });

  it("surfaces retry failures without clearing the run view", async () => {
    const ipc = fakeIpc();
    vi.mocked(ipc.retryNode).mockRejectedValueOnce(new Error("retry failed"));
    render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
    await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
    fireEvent.click(screen.getByTestId("workflow-item"));
    await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));
    fireEvent.click(screen.getByText("运行"));
    await screen.findByTestId("workflow-node-retry");
    fireEvent.click(screen.getByTestId("workflow-node-retry"));
    expect((await screen.findByTestId("workflow-error")).textContent).toContain(
      "retry failed",
    );
    expect(screen.getByTestId("workflow-run-view").getAttribute("data-run-id")).toBe(
      RUN,
    );
  });

  it.each(["interrupted", "skipped", "timeout", "mystery"] as const)(
    "renders %s run status labels",
    async (status) => {
      const runDetail: WorkflowRunDetail = {
        workflow_run_id: RUN,
        workflow_id: WORKFLOW,
        session_id: SESSION,
        status,
        priority: 0,
        started_at: 10,
        finished_at: 20,
        error: null,
        nodes: [],
      };
      const ipc = fakeIpc({ runGet: vi.fn(async () => runDetail) });
      render(<WorkflowsPage ipc={ipc} onBack={() => {}} sessionId={SESSION} />);
      await waitFor(() => expect(screen.getByTestId("workflow-item")).toBeTruthy());
      fireEvent.click(screen.getByTestId("workflow-item"));
      await waitFor(() => expect(ipc.get).toHaveBeenCalledWith(WORKFLOW));
      fireEvent.click(screen.getByText("运行"));
      const view = await screen.findByTestId("workflow-run-view");
      await waitFor(() => expect(view.getAttribute("data-status")).toBe(status));
      expect(view.textContent).not.toContain("尚未运行");
    },
  );
});
