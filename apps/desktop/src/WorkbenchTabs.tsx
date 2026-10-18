/**
 * 工作台标签栏（M7-04；UI-01 完整、设计 §2.3「多视图」P1、§4#20）。
 *
 * 锚点契约（UI-UX §7.3，M7-04 任务登记）：
 * - 容器 `workbench-tabs`；
 * - 标签 `session-tab`（`data-session-id` / `data-active` / `data-status` /
 *   `data-background`）；`data-status` = M3-12 分组 id（`sessionGroupOf`），
 *   **后台会话状态角标与分组口径一致**（DoD4）；
 * - 角标 `session-tab-badge`（`data-group` = running/waiting_permission/failed；
 *   其他组不渲染；空组口径同 M3-12「空组隐藏」）；
 * - 关闭 `session-tab-close`；分屏开关 `split-toggle`（`data-enabled` / `data-split`）。
 */
import type { SessionSummary } from "./session";
import { SESSION_STATUS_LABELS } from "./session";
import { SESSION_GROUP_TITLES, sessionGroupOf } from "./sessionGroups";

export interface WorkbenchTabsProps {
  /** 打开的会话（按标签顺序；调用方已过滤不存在会话）。 */
  sessions: SessionSummary[];
  activeSessionId: string | null;
  splitSessionId: string | null;
  onActivate: (sessionId: string) => void;
  onClose: (sessionId: string) => void;
  onToggleSplit: () => void;
  /** 分屏开关可用（已有分屏可关闭，或存在可放入第二窗格的其他标签）。 */
  splitEnabled: boolean;
}

export function WorkbenchTabs({
  sessions,
  activeSessionId,
  splitSessionId,
  onActivate,
  onClose,
  onToggleSplit,
  splitEnabled,
}: WorkbenchTabsProps) {
  return (
    <div className="workbench-tabs" data-testid="workbench-tabs">
      <div className="workbench-tab-strip" role="tablist">
        {sessions.map((session) => {
          const group = sessionGroupOf(session.status);
          const active = session.id === activeSessionId;
          // DoD4：后台 = 未显示在任何窗格（主窗格聚焦会话 / 分屏第二窗格）。
          const background = !active && session.id !== splitSessionId;
          return (
            <div
              key={session.id}
              role="tab"
              aria-selected={active}
              tabIndex={0}
              className={active ? "session-tab active" : "session-tab"}
              data-testid="session-tab"
              data-session-id={session.id}
              data-active={String(active)}
              data-status={group}
              data-background={String(background)}
              onClick={() => onActivate(session.id)}
              onKeyDown={(event) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  onActivate(session.id);
                }
              }}
            >
              <span className="session-tab-title" data-testid="session-tab-title">
                {session.title}
              </span>
              <span className="session-tab-status" title={SESSION_STATUS_LABELS[session.status]}>
                {SESSION_STATUS_LABELS[session.status]}
              </span>
              {group !== "other" ? (
                <span
                  className="session-tab-badge"
                  data-testid="session-tab-badge"
                  data-group={group}
                  title={`${SESSION_GROUP_TITLES[group]}（后台状态角标；与 M3-12 分组口径一致）`}
                >
                  {SESSION_GROUP_TITLES[group]}
                </span>
              ) : null}
              <button
                type="button"
                className="session-tab-close"
                data-testid="session-tab-close"
                data-session-id={session.id}
                aria-label={`关闭标签：${session.title}`}
                title="关闭标签（不删除会话）"
                onClick={(event) => {
                  event.stopPropagation();
                  onClose(session.id);
                }}
              >
                ×
              </button>
            </div>
          );
        })}
        {sessions.length === 0 ? (
          <span className="workbench-tabs-empty" data-testid="workbench-tabs-empty">
            未打开会话（从左侧会话列表选择以打开标签）
          </span>
        ) : null}
      </div>
      <button
        type="button"
        role="switch"
        aria-checked={splitSessionId !== null}
        className={splitSessionId !== null ? "split-toggle active" : "split-toggle"}
        data-testid="split-toggle"
        data-enabled={String(splitEnabled)}
        data-split={String(splitSessionId !== null)}
        disabled={!splitEnabled}
        title={
          splitSessionId !== null
            ? "取消分屏（回到单窗格标签视图）"
            : splitEnabled
              ? "分屏：在右侧第二窗格显示另一个已打开的会话"
              : "分屏需要至少两个已打开标签"
        }
        onClick={onToggleSplit}
      >
        {splitSessionId !== null ? "取消分屏" : "分屏"}
      </button>
    </div>
  );
}
