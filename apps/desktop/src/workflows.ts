/**
 * 工作流编排 IPC 契约（M10-01/ADR-031；UI-UX S-14）。
 *
 * 覆盖：列表、JSON 定义读取/保存/导入、运行、运行视图/节点状态、取消、节点重试。
 * 无拖拽画布（UI-04 画布仍属 P4）。
 */
import { client } from "@aether/protocol";

export interface WorkflowSummary {
  workflow_id: string;
  name: string;
  description: string | null;
  version: number;
  is_template: boolean;
  created_at: number;
  updated_at: number;
}

export interface WorkflowDetail extends WorkflowSummary {
  definition: unknown;
  definition_hash?: string;
}

export interface WorkflowRunSummary {
  workflow_run_id: string;
  workflow_id: string;
  session_id: string | null;
  status: "queued" | "running" | "succeeded" | "failed" | "cancelled" | "interrupted" | string;
  priority: number;
  started_at: number;
  finished_at: number | null;
  error: string | null;
  definition_version?: number;
  definition_hash?: string | null;
}

export interface WorkflowNodeStatus {
  node_run_id: string;
  node_id: string;
  type: string;
  status: string;
  attempt: number;
  started_at: number | null;
  finished_at: number | null;
  error: string | null;
  result_summary: string | null;
}

export interface WorkflowRunDetail extends WorkflowRunSummary {
  nodes?: WorkflowNodeStatus[];
}

export interface WorkflowListResult {
  items: WorkflowSummary[];
  total: number;
}

export interface WorkflowRunListResult {
  items: WorkflowRunSummary[];
  total: number;
}

export interface WorkflowIpc {
  list(): Promise<WorkflowListResult>;
  get(workflowId: string): Promise<WorkflowDetail>;
  create(input: {
    name: string;
    description?: string;
    definition: unknown;
    is_template?: boolean;
  }): Promise<WorkflowSummary>;
  update(input: {
    workflow_id: string;
    expected_version: number;
    name?: string;
    description?: string;
    definition?: unknown;
    is_template?: boolean;
  }): Promise<WorkflowDetail>;
  remove(workflowId: string, force?: boolean): Promise<{ deleted: boolean }>;
  importWorkflow(sourcePath: string, nameOverride?: string): Promise<WorkflowDetail>;
  run(input: {
    workflow_id: string;
    session_id: string;
    input?: unknown;
    priority?: number;
  }): Promise<{ workflow_run_id: string; status: string }>;
  runGet(workflowRunId: string, includeNodes?: boolean): Promise<WorkflowRunDetail>;
  runList(input: {
    workflow_id?: string;
    session_id?: string;
    status?: string;
    limit?: number;
    offset?: number;
  }): Promise<WorkflowRunListResult>;
  cancel(workflowRunId: string, reason?: string): Promise<{ status: string }>;
  retryNode(
    workflowRunId: string,
    nodeId: string,
    resetDownstream?: boolean,
  ): Promise<{ node_run_id: string; attempt: number }>;
}

export const workflowIpc: WorkflowIpc = {
  list: () => client.workflowList<WorkflowListResult>({ limit: 200 }),
  get: (workflowId) =>
    client.workflowGet<WorkflowDetail>({ workflow_id: workflowId }),
  create: (input) => client.workflowCreate<WorkflowSummary>(input),
  update: (input) => client.workflowUpdate<WorkflowDetail>(input),
  remove: (workflowId, force) =>
    client.workflowDelete<{ deleted: boolean }>({ workflow_id: workflowId, force }),
  importWorkflow: (sourcePath, nameOverride) =>
    client.workflowImport<WorkflowDetail>({
      source_path: sourcePath,
      name_override: nameOverride,
    }),
  run: (input) =>
    client.workflowRun<{ workflow_run_id: string; status: string }>(input),
  runGet: (workflowRunId, includeNodes = true) =>
    client.workflowRunGet<WorkflowRunDetail>({
      workflow_run_id: workflowRunId,
      include_nodes: includeNodes,
    }),
  runList: (input) => client.workflowRunList<WorkflowRunListResult>(input),
  cancel: (workflowRunId, reason) =>
    client.workflowRunCancel<{ status: string }>({
      workflow_run_id: workflowRunId,
      reason,
    }),
  retryNode: (workflowRunId, nodeId, resetDownstream = false) =>
    client.workflowNodeRetry<{ node_run_id: string; attempt: number }>({
      workflow_run_id: workflowRunId,
      node_id: nodeId,
      reset_downstream: resetDownstream,
    }),
};
