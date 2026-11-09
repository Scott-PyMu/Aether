/**
 * M10-02/ADR-032：子 Agent 命令面（UI-03 子会话树 / 结果卡）。
 *
 * 生产实现 = `@aether/protocol` typed client；测试可注入替身。
 * 唯一树入口 = `subagent_list`；子会话详情不是独立根路由。
 */
import { client } from "@aether/protocol";

export type SubagentInjectionState =
  | "pending"
  | "injected"
  | "folded"
  | "rejected"
  | "superseded";

export type SubagentDeliveryMode =
  | "message_append"
  | "origin_fork"
  | "system_replace";

export interface SubagentNode {
  child_session_id: string;
  parent_session_id: string;
  title: string;
  role: string;
  mode: string;
  depth: number;
  status: string;
  verdict?: string | null;
  result_summary?: string | null;
  summary_truncated?: boolean;
  result_ref?: string | null;
  injection_state: SubagentInjectionState;
  delivery_mode: SubagentDeliveryMode;
  has_more_details: boolean;
  created_at: number;
}

export interface SubagentListResult {
  items: SubagentNode[];
}

export interface SubagentResolveResult {
  decision: "accept" | "reject" | "rerun";
  injected: boolean;
  injection_id?: string;
  context_version: number;
  mode: string;
  new_child_session_id?: string;
}

export interface SubagentResultChunk {
  child_session_id: string;
  result_chunk: string;
  offset: number;
  next_offset: number | null;
  total_bytes: number;
  truncated: boolean;
}

export interface SubagentSpawnInput {
  parent_session_id: string;
  role: "explore" | "review" | "custom";
  purpose: string;
  mode?: "explore" | "review" | "custom";
  template?: "parallel_explore" | "adversarial_review" | "custom";
  context_refs?: Array<{ kind: "text" | "file" | "directory"; path?: string; text?: string }>;
  runtime_id?: string;
  provider_id?: string;
  model?: string;
  permission_mode?: "read_only" | "workspace_write" | "danger_full_access";
  timeout_ms?: number;
}

export interface SubagentIpc {
  list(parentSessionId: string, includeCompleted?: boolean): Promise<SubagentListResult>;
  spawn(input: SubagentSpawnInput): Promise<{
    child_session_id: string;
    status: string;
    depth: number;
    mode: string;
    batch_id: string;
  }>;
  cancel(childSessionId: string, reason?: string): Promise<{ status: string }>;
  resolve(
    childSessionId: string,
    decision: "accept" | "reject" | "rerun",
    note?: string,
  ): Promise<SubagentResolveResult>;
  resultGet(
    parentSessionId: string,
    childSessionId: string,
    offset?: number,
    limitBytes?: number,
  ): Promise<SubagentResultChunk>;
}

/** 生产实现（typed client；命令入参为内层 DTO）。 */
export const subagentIpc: SubagentIpc = {
  async list(parentSessionId, includeCompleted = true) {
    return client.subagentList<SubagentListResult>({
      parent_session_id: parentSessionId,
      include_completed: includeCompleted,
    });
  },
  async spawn(input) {
    return client.subagentSpawn<{
      child_session_id: string;
      status: string;
      depth: number;
      mode: string;
      batch_id: string;
    }>(input);
  },
  async cancel(childSessionId, reason) {
    return client.subagentCancel<{ status: string }>({
      child_session_id: childSessionId,
      ...(reason ? { reason } : {}),
    });
  },
  async resolve(childSessionId, decision, note) {
    return client.subagentResolve<SubagentResolveResult>({
      child_session_id: childSessionId,
      decision,
      ...(note ? { note } : {}),
    });
  },
  async resultGet(parentSessionId, childSessionId, offset = 0, limitBytes = 16_384) {
    return client.subagentResultGet<SubagentResultChunk>({
      parent_session_id: parentSessionId,
      child_session_id: childSessionId,
      offset,
      limit_bytes: limitBytes,
    });
  },
};

export const SUBAGENT_INJECTION_LABELS: Record<SubagentInjectionState, string> = {
  pending: "待处理",
  injected: "已回注",
  folded: "已折叠",
  rejected: "已拒绝",
  superseded: "已被替代",
};

export const SUBAGENT_VERDICT_LABELS: Record<string, string> = {
  approve: "通过",
  reject: "拒绝",
  needs_changes: "需修改",
  info: "信息",
};
