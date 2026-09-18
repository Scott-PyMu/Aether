/**
 * M1-09 DoD 第 6 条：Mock 内置 5 类工具调用注入清单（**权威定义**，供 M2-02/M2-10 复用）。
 *
 * | 场景 | 触发文本 | 事件序列（type） |
 * |---|---|---|
 * | ① 正常完成 | `tool:normal` | `tool.call_started` → `tool.call_completed` |
 * | ② 执行失败 | `tool:fail` | `tool.call_started` → `tool.call_failed`（error） |
 * | ③ 超时中断 | `tool:timeout` | `tool.call_started` → `tool.call_failed`（timeout/abort） |
 * | ④ 权限 ask→允许 | `tool:permission-allow` | `permission.requested` → `permission.resolved(allow)` + `tool.call_completed` |
 * | ⑤ 权限 ask→拒绝 | `tool:permission-deny` | `permission.requested` → `permission.resolved(deny)` + `tool.call_failed`（denied） |
 *
 * 说明（M1 阶段口径，与实施计划 M1-09 DoD6 / M2-10 一致）：
 * - ④⑤ 在 M1 阶段为**事件序列预置**：Mock 自包含产出
 *   `permission.requested` → `permission.resolved` → 工具终态，**不发送**
 *   `permission.request` 通知、不等待 `permission.resolve`、不经核心权限网关
 *   （边界 B1，见 `docs/M1-09-证据.md`；M2 阶段由 M2-10 在真实回环中重放验证）。
 * - 预置决策值固定在 [`TOOL_CALL_SCENARIOS`]（禁止由测试侧注入，保证「Mock 侧直接产出」）。
 * - ④⑤ 的事件序列按 DoD 字面定义从 `permission.requested` 起（不含 `tool.call_started`）。
 * - 每个场景包在 run 生命周期内：`run.started` →（工具/权限序列）→ `run.completed`
 *   （③ 在中断后以 `run.cancelled` 收口）。
 */

export const TOOL_CALL_SCENARIOS = {
  normal: {
    id: "normal",
    trigger: "tool:normal",
    label: "① 正常完成",
    events: ["tool.call_started", "tool.call_completed"],
    toolName: "mock.echo",
  },
  fail: {
    id: "fail",
    trigger: "tool:fail",
    label: "② 执行失败",
    events: ["tool.call_started", "tool.call_failed"],
    toolName: "mock.read_file",
  },
  timeout: {
    id: "timeout",
    trigger: "tool:timeout",
    label: "③ 超时中断",
    events: ["tool.call_started", "tool.call_failed"],
    toolName: "mock.read_file",
  },
  permission_allow: {
    id: "permission_allow",
    trigger: "tool:permission-allow",
    label: "④ 权限 ask→允许",
    events: ["permission.requested", "permission.resolved", "tool.call_completed"],
    decision: "allow",
    toolName: "mock.write_file",
  },
  permission_deny: {
    id: "permission_deny",
    trigger: "tool:permission-deny",
    label: "⑤ 权限 ask→拒绝",
    events: ["permission.requested", "permission.resolved", "tool.call_failed"],
    decision: "deny",
    toolName: "mock.write_file",
  },
} as const;

export type ToolCallScenarioId = keyof typeof TOOL_CALL_SCENARIOS;

export function scenarioForText(text: string): ToolCallScenarioId | undefined {
  const normalized = text.trim();
  for (const [id, scenario] of Object.entries(TOOL_CALL_SCENARIOS)) {
    if (normalized === scenario.trigger) return id as ToolCallScenarioId;
  }
  return undefined;
}

export interface PermissionRequestPayload {
  request_id: string;
  resource: string;
  action: string;
  target: string;
}

export function buildToolCallStarted(
  toolCallId: string,
  toolName: string,
  args: Record<string, unknown>,
): Record<string, unknown> {
  return { tool_call_id: toolCallId, tool_name: toolName, args };
}

export function buildToolCallCompleted(
  toolCallId: string,
  toolName: string,
  durationMs: number,
): Record<string, unknown> {
  return { tool_call_id: toolCallId, tool_name: toolName, duration_ms: durationMs };
}

export function buildToolCallFailed(
  toolCallId: string,
  toolName: string,
  durationMs: number,
  error: { code: string; message: string; recoverable: boolean },
): Record<string, unknown> {
  return {
    tool_call_id: toolCallId,
    tool_name: toolName,
    duration_ms: durationMs,
    error,
  };
}

export function buildPermissionRequested(payload: PermissionRequestPayload): Record<string, unknown> {
  return {
    request_id: payload.request_id,
    resource: payload.resource,
    action: payload.action,
    target: payload.target,
  };
}

export function buildPermissionResolved(
  requestId: string,
  decision: "allow" | "deny",
  scope: "once" | "session",
): Record<string, unknown> {
  return { request_id: requestId, decision, scope };
}

export const TOOL_FAILURE_ERROR = {
  code: "tool_execution_failed",
  message: "mock 工具执行失败（注入）",
  recoverable: true,
} as const;

export const TOOL_TIMEOUT_ERROR = {
  code: "timeout",
  message: "mock 工具调用超时并被中断（abort）",
  recoverable: true,
} as const;

export const PERMISSION_DENIED_ERROR = {
  code: "denied",
  message: "权限被拒绝（mock 注入）",
  recoverable: false,
} as const;
