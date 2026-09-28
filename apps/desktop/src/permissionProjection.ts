/**
 * 权限事件 × 待审批清单 → 权限中心视图投影（M3-03；D9）。
 *
 * 数据来源（与 UI-UX §2.5 口径一致）：
 * - **清单校准**（`permissions_pending`）：打开面板/会话切换/权限事件到达时全量校准；
 * - **事件流增量**（`permission.requested` / `permission.resolved`，EventStore）：
 *   维护决议状态与超时呈现，避免新增轮询。
 *
 * 状态判定：`timeout` = 决议为 deny 且决议时间不早于 `requested_at + timeout_ms`
 * （300s 自动拒绝；用户 deny/取消早于截止时保持 `resolved`）。
 */
import type { AetherEvent } from "@aether/protocol";

import { APPROVAL_TIMEOUT_MS, type PendingPermission } from "./permission";

export type PermissionItemStatus = "pending" | "resolved" | "timeout";

export interface PermissionItemView {
  requestId: string;
  sessionId: string | null;
  resource: string;
  action: string;
  target: string | null;
  canonicalTarget: string | null;
  requestedAt: number;
  timeoutMs: number;
  status: PermissionItemStatus;
  decision: "allow" | "deny" | null;
  scope: "once" | "session" | null;
  resolvedAt: number | null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

/**
 * 原文与规范化结果是否等价（`data-equal` 语义；D9 对照展示的判定）。
 *
 * Windows `canonicalize` 返回 `\\?\` 扩展前缀；剥离后按大小写不敏感比较，
 * 避免把「同一路径的规范化形式」误报为不一致（视觉欺骗提示只针对真实差异）。
 */
export function targetsEquivalent(
  raw: string | null,
  canonical: string | null,
): boolean | null {
  if (raw === null || canonical === null) {
    return null;
  }
  if (raw === canonical) {
    return true;
  }
  const strip = (value: string) =>
    value.replace(/^\\\\\?\\/, "").replace(/\\+$/, "").toLowerCase();
  return strip(raw) === strip(canonical);
}

/** 投影权限中心视图（纯函数；输入为清单与已发布事件）。 */
export function projectPermissions(
  events: AetherEvent[],
  pending: PendingPermission[],
): PermissionItemView[] {
  const items = new Map<string, PermissionItemView>();

  for (const item of pending) {
    items.set(item.request_id, {
      requestId: item.request_id,
      sessionId: item.session_id,
      resource: item.resource,
      action: item.action,
      target: item.target,
      canonicalTarget: item.canonical_target,
      requestedAt: item.requested_at,
      timeoutMs: item.timeout_ms > 0 ? item.timeout_ms : APPROVAL_TIMEOUT_MS,
      status: "pending",
      decision: null,
      scope: null,
      resolvedAt: null,
    });
  }

  for (const event of events) {
    if (event.type !== "permission.requested" && event.type !== "permission.resolved") {
      continue;
    }
    const payload = asRecord(event.payload);
    const requestId = asString(payload.request_id);
    if (!requestId) {
      continue;
    }
    const existing = items.get(requestId);
    if (event.type === "permission.requested") {
      if (!existing) {
        items.set(requestId, {
          requestId,
          sessionId: event.session_id ?? null,
          resource: asString(payload.resource) ?? "unknown",
          action: asString(payload.action) ?? "unknown",
          target: asString(payload.target),
          canonicalTarget: null,
          requestedAt: event.ts,
          timeoutMs: APPROVAL_TIMEOUT_MS,
          status: "pending",
          decision: null,
          scope: null,
          resolvedAt: null,
        });
      }
      continue;
    }
    // permission.resolved
    const decision = asString(payload.decision);
    const scope = asString(payload.scope);
    const previous: PermissionItemView = existing ?? {
      requestId,
      sessionId: event.session_id ?? null,
      resource: "unknown",
      action: "unknown",
      target: null,
      canonicalTarget: null,
      requestedAt: event.ts,
      timeoutMs: APPROVAL_TIMEOUT_MS,
      status: "pending",
      decision: null,
      scope: null,
      resolvedAt: null,
    };
    const timeoutMs =
      previous.timeoutMs > 0 ? previous.timeoutMs : APPROVAL_TIMEOUT_MS;
    const timedOut =
      decision === "deny" && event.ts - previous.requestedAt >= timeoutMs;
    items.set(requestId, {
      ...previous,
      status: timedOut ? "timeout" : "resolved",
      decision: decision === "allow" ? "allow" : decision === "deny" ? "deny" : null,
      scope: scope === "once" || scope === "session" ? scope : null,
      resolvedAt: event.ts,
    });
  }

  return [...items.values()].sort((left, right) => {
    if (left.requestedAt !== right.requestedAt) {
      return left.requestedAt - right.requestedAt;
    }
    return left.requestId.localeCompare(right.requestId);
  });
}
