/**
 * 会话列表分组（M3-12/ADR-011；UI-UX §2.2）。
 *
 * 固定组序：运行中（running/creating）→ 等待审批（waiting_permission）→
 * 失败（failed）→ 其他（idle/completed/cancelled）；组内按 `updated_at` 倒序；
 * 空组隐藏。锚点契约（UI-UX §7.3 冻结）：
 * - 分组容器：`data-testid="session-group"` + `data-group`（四组 id）；
 * - 分组标题：`data-testid="session-group-title"`；
 * - 既有锚点（`session-list` / `session-item` / `session-item-status` /
 *   `data-session-id` / `data-active`）不重命名。
 */
import type { SessionStatus, SessionSummary } from "./session";

/** 会话列表分组 id（`data-group` 取值；固定组序）。 */
export type SessionGroupId = "running" | "waiting_permission" | "failed" | "other";

/** 固定组序（UI-UX §2.2）。 */
export const SESSION_GROUP_ORDER: readonly SessionGroupId[] = [
  "running",
  "waiting_permission",
  "failed",
  "other",
];

/** 分组标题文案（断言不依赖文案；UI-UX §2.2）。 */
export const SESSION_GROUP_TITLES: Record<SessionGroupId, string> = {
  running: "运行中",
  waiting_permission: "等待审批",
  failed: "失败",
  other: "其他",
};

/** 会话状态 → 分组（running/creating 并入「运行中」）。 */
export function sessionGroupOf(status: SessionStatus): SessionGroupId {
  switch (status) {
    case "running":
    case "creating":
      return "running";
    case "waiting_permission":
      return "waiting_permission";
    case "failed":
      return "failed";
    default:
      return "other";
  }
}

/** 分组后的会话列表（固定组序；空组不渲染；组内 `updated_at` 倒序）。 */
export function groupSessions(
  sessions: readonly SessionSummary[],
): Array<{ group: SessionGroupId; sessions: SessionSummary[] }> {
  const buckets = new Map<SessionGroupId, SessionSummary[]>();
  for (const group of SESSION_GROUP_ORDER) {
    buckets.set(group, []);
  }
  for (const session of sessions) {
    buckets.get(sessionGroupOf(session.status))?.push(session);
  }
  return SESSION_GROUP_ORDER.map((group) => ({
    group,
    sessions: [...(buckets.get(group) ?? [])].sort(
      (left, right) => right.updated_at - left.updated_at,
    ),
  })).filter((entry) => entry.sessions.length > 0);
}
