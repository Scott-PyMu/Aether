import { afterEach, describe, expect, it, vi } from "vitest";

import { client } from "@aether/protocol";

import { workflowIpc } from "./workflows";

const WORKFLOW = "01JWORKFLOW00000000000000";
const RUN = "01JWORKFLOWRUN00000000000";
const NODE = "01JWORKFLOWNODE0000000000";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("M10-01 workflowIpc 命令封装", () => {
  it("list/get/create/update/remove/import 映射到 typed client", async () => {
    const list = vi.spyOn(client, "workflowList").mockResolvedValue({
      items: [],
      total: 0,
    });
    const get = vi.spyOn(client, "workflowGet").mockResolvedValue({
      workflow_id: WORKFLOW,
      name: "wf",
      description: null,
      version: 1,
      is_template: false,
      created_at: 1,
      updated_at: 1,
      definition: {},
    });
    const create = vi.spyOn(client, "workflowCreate").mockResolvedValue({
      workflow_id: WORKFLOW,
      name: "wf",
      description: null,
      version: 1,
      is_template: false,
      created_at: 1,
      updated_at: 1,
    });
    const update = vi.spyOn(client, "workflowUpdate").mockResolvedValue({
      workflow_id: WORKFLOW,
      name: "wf2",
      description: null,
      version: 2,
      is_template: false,
      created_at: 1,
      updated_at: 2,
      definition: {},
    });
    const remove = vi
      .spyOn(client, "workflowDelete")
      .mockResolvedValue({ deleted: true });
    const importWorkflow = vi.spyOn(client, "workflowImport").mockResolvedValue({
      workflow_id: WORKFLOW,
      name: "imported",
      description: null,
      version: 1,
      is_template: false,
      created_at: 1,
      updated_at: 1,
      definition: {},
    });

    await expect(workflowIpc.list()).resolves.toEqual({ items: [], total: 0 });
    expect(list).toHaveBeenCalledWith({ limit: 200 });

    await workflowIpc.get(WORKFLOW);
    expect(get).toHaveBeenCalledWith({ workflow_id: WORKFLOW });

    const definition = { nodes: [] };
    await workflowIpc.create({ name: "wf", definition });
    expect(create).toHaveBeenCalledWith({ name: "wf", definition });

    await workflowIpc.update({
      workflow_id: WORKFLOW,
      expected_version: 1,
      name: "wf2",
    });
    expect(update).toHaveBeenCalledWith({
      workflow_id: WORKFLOW,
      expected_version: 1,
      name: "wf2",
    });

    await expect(workflowIpc.remove(WORKFLOW)).resolves.toEqual({
      deleted: true,
    });
    expect(remove).toHaveBeenCalledWith({
      workflow_id: WORKFLOW,
      force: undefined,
    });
    await workflowIpc.remove(WORKFLOW, true);
    expect(remove).toHaveBeenLastCalledWith({
      workflow_id: WORKFLOW,
      force: true,
    });

    await workflowIpc.importWorkflow("D:/wf.json");
    expect(importWorkflow).toHaveBeenCalledWith({
      source_path: "D:/wf.json",
      name_override: undefined,
    });
    await workflowIpc.importWorkflow("D:/wf.json", "imported");
    expect(importWorkflow).toHaveBeenLastCalledWith({
      source_path: "D:/wf.json",
      name_override: "imported",
    });
  });

  it("run/runGet/runList/cancel/retryNode 映射到 typed client", async () => {
    const run = vi.spyOn(client, "workflowRun").mockResolvedValue({
      workflow_run_id: RUN,
      status: "queued",
    });
    const runGet = vi.spyOn(client, "workflowRunGet").mockResolvedValue({
      workflow_run_id: RUN,
      workflow_id: WORKFLOW,
      session_id: "01JSESSION000000000000000",
      status: "running",
      priority: 0,
      started_at: 1,
      finished_at: null,
      error: null,
    });
    const runList = vi.spyOn(client, "workflowRunList").mockResolvedValue({
      items: [],
      total: 0,
    });
    const cancel = vi.spyOn(client, "workflowRunCancel").mockResolvedValue({
      status: "cancelled",
    });
    const retryNode = vi.spyOn(client, "workflowNodeRetry").mockResolvedValue({
      node_run_id: NODE,
      attempt: 2,
    });

    const runInput = {
      workflow_id: WORKFLOW,
      session_id: "01JSESSION000000000000000",
      input: { topic: "x" },
      priority: 1,
    };
    await workflowIpc.run(runInput);
    expect(run).toHaveBeenCalledWith(runInput);

    await workflowIpc.runGet(RUN);
    expect(runGet).toHaveBeenCalledWith({
      workflow_run_id: RUN,
      include_nodes: true,
    });
    await workflowIpc.runGet(RUN, false);
    expect(runGet).toHaveBeenLastCalledWith({
      workflow_run_id: RUN,
      include_nodes: false,
    });

    const listInput = { workflow_id: WORKFLOW, limit: 10, offset: 0 };
    await workflowIpc.runList(listInput);
    expect(runList).toHaveBeenCalledWith(listInput);

    await workflowIpc.cancel(RUN);
    expect(cancel).toHaveBeenCalledWith({
      workflow_run_id: RUN,
      reason: undefined,
    });
    await workflowIpc.cancel(RUN, "user");
    expect(cancel).toHaveBeenLastCalledWith({
      workflow_run_id: RUN,
      reason: "user",
    });

    await workflowIpc.retryNode(RUN, "node-a");
    expect(retryNode).toHaveBeenCalledWith({
      workflow_run_id: RUN,
      node_id: "node-a",
      reset_downstream: false,
    });
    await workflowIpc.retryNode(RUN, "node-a", true);
    expect(retryNode).toHaveBeenLastCalledWith({
      workflow_run_id: RUN,
      node_id: "node-a",
      reset_downstream: true,
    });
  });
});
