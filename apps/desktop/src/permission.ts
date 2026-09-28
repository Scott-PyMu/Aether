/**
 * 权限中心与运行时控制 IPC 契约（M3-03；设计 D9/D5，UI-UX S-03/S-04）。
 *
 * 与 Rust 侧 `session_backend.rs` 的 JSON 形状一致（snake_case）：
 * - `permissions_pending` → 待审批清单（原文 target + canonical 对照字段；D9 评审 #10）；
 * - `permission_resolve` → 用户决议（`once` / `session` / `deny`；D9）；
 * - `runtime_retry` / `runtime_enable` → M1-10 监督器控制命令（M2-01 已接线）。
 *
 * 本文件是工作台之外「权限/运行时」命令面的类型化封装（可注入替身；生产 = Tauri
 * invoke）。命令名与生成绑定（`packages/protocol/src/bindings.ts`）一致。
 */
import { invoke } from "@tauri-apps/api/core";

/** D9：审批超时（300s；响应携带 `timeout_ms` 时以其为准，常量仅作兜底）。 */
export const APPROVAL_TIMEOUT_MS = 300_000;

/** 用户决议（D9：`once` / `session` 授权；`deny` 拒绝）。 */
export type PermissionDecision = "once" | "session" | "deny";

/** `permissions_pending` 清单条目（按 `requested_at` 升序 = 审批队列顺序）。 */
export interface PendingPermission {
  id: string;
  request_id: string;
  session_id: string | null;
  resource: string;
  action: string;
  /** 原始 target（未规范化；UI 展示原文，D9 防视觉欺骗）。 */
  target: string | null;
  /** canonical 结果（路径类资源；已决议/重启恢复的票据可能为 null）。 */
  canonical_target: string | null;
  requested_at: number;
  timeout_ms: number;
}

/** `permission_resolve` 回执。 */
export interface PermissionResolveResult {
  request_id: string;
  decision: "allow" | "deny";
  scope: "once" | "session" | null;
  ticket_id: string;
}

/** `runtime_retry` / `runtime_enable` 回执（M1-10 `StartOutcome` 的 JSON 形状）。 */
export interface RuntimeControlResult {
  runtime_id: string;
  /** `ready` / `already_running` / `failed` / `rejected`。 */
  outcome: string;
  status: string;
  status_reason?: string | null;
  detail?: string | null;
  stderr_tail?: string[];
}

/**
 * 权限/运行时命令面（可注入替身；生产 = Tauri 生产实现）。
 *
 * 缺省/未接线时由调用方处理（面板显示空态或错误；不伪造 pending）。
 */
export interface PermissionIpc {
  /** 待审批清单（缺省 = 全部会话；UI 侧按会话过滤并维护全局面板计数）。 */
  pendingPermissions(sessionId?: string): Promise<PendingPermission[]>;
  resolvePermission(
    requestId: string,
    decision: PermissionDecision,
  ): Promise<PermissionResolveResult>;
  retryRuntime(runtimeId: string): Promise<RuntimeControlResult>;
  enableRuntime(runtimeId: string): Promise<RuntimeControlResult>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const permissionIpc: PermissionIpc = {
  async pendingPermissions(sessionId) {
    return invoke<PendingPermission[]>("permissions_pending", {
      payload: sessionId ? { session_id: sessionId } : {},
    });
  },
  async resolvePermission(requestId, decision) {
    return invoke<PermissionResolveResult>("permission_resolve", {
      payload: { request_id: requestId, decision },
    });
  },
  async retryRuntime(runtimeId) {
    return invoke<RuntimeControlResult>("runtime_retry", {
      payload: { runtime_id: runtimeId },
    });
  },
  async enableRuntime(runtimeId) {
    return invoke<RuntimeControlResult>("runtime_enable", {
      payload: { runtime_id: runtimeId },
    });
  },
};

/** `runtime_retry`/`runtime_enable` 结果 outcome 展示文案。 */
export const RUNTIME_OUTCOME_LABELS: Record<string, string> = {
  ready: "已就绪",
  already_running: "已在运行",
  failed: "启动失败",
  rejected: "被拒绝",
};
