/**
 * 工作台顶栏（M3-03；UI-UX §2.5/Q8：合并 M3-02 状态条 + 运行/存储/待审批摘要）。
 *
 * - 左：应用名 + 当前会话标题 + 会话状态 chip（`session-status`）；
 * - 中：run 状态 + 中断入口 + 最近 ack（原状态条 testid 保持不变，Q8「避免双状态条」）；
 * - 右：运行时紧凑徽标组（`runtime-badge`，hover 显示 reason 中文释义）、存储状态
 *   （`storage-indicator`）、待审批计数（`pending-permission-badge`）、右栏抽屉开关
 *   （<1280px 断点由 CSS 控制显示，见 styles.css）。
 */
import { useStorageHealth } from "./healthBus";
import {
  RUNTIME_REASON_LABELS,
  RUNTIME_STATUS_LABELS,
  SESSION_STATUS_LABELS,
  type RuntimeInfo,
  type SessionSummary,
} from "./session";

export interface TopBarProps {
  session: SessionSummary | null;
  runId: string | null;
  runStatusText: string;
  interruptible: boolean;
  onInterrupt: () => void;
  lastAck: { runId: string; duplicate: boolean } | null;
  runtimes: RuntimeInfo[];
  pendingCount: number;
  rightOpen: boolean;
  onToggleRight: () => void;
}

/** 顶栏存储指示（消费共享健康总线；`persist_degraded` 与横幅同一事实源）。 */
function StorageIndicator() {
  const health = useStorageHealth();
  const state =
    health.status === "degraded"
      ? "persist_degraded"
      : health.status === "loading"
        ? "loading"
        : health.status === "unresponsive"
          ? "unresponsive"
          : "normal";
  const labels: Record<string, string> = {
    persist_degraded: "降级",
    loading: "查询中",
    unresponsive: "未响应",
    normal: "正常",
  };
  return (
    <span
      className={`storage-indicator storage-${state}`}
      data-testid="storage-indicator"
      data-status={state}
      title={state === "persist_degraded" ? "存储降级（只读）；详见降级横幅" : `存储${labels[state]}`}
    >
      存储：{labels[state]}
    </span>
  );
}

export function TopBar({
  session,
  runId,
  runStatusText,
  interruptible,
  onInterrupt,
  lastAck,
  runtimes,
  pendingCount,
  rightOpen,
  onToggleRight,
}: TopBarProps) {
  return (
    <header className="workbench-topbar" data-testid="topbar">
      <div className="topbar-left">
        <span className="topbar-app">Aether</span>
        {session ? (
          <>
            <span className="topbar-session-title">{session.title}</span>
            <span
              className="topbar-session-status"
              data-testid="session-status"
              data-status={session.status}
            >
              {SESSION_STATUS_LABELS[session.status]}
            </span>
          </>
        ) : (
          <span className="topbar-session-title muted">未选择会话</span>
        )}
      </div>

      <div className="topbar-middle">
        {session ? (
          <div className="session-status-bar" data-testid="status-bar">
            <span data-testid="run-status" data-run-id={runId ?? ""}>
              {runStatusText}
            </span>
            <button
              type="button"
              data-testid="interrupt"
              disabled={!interruptible}
              onClick={onInterrupt}
            >
              中断
            </button>
            {lastAck ? (
              <span className="last-ack" data-testid="last-ack" data-run-id={lastAck.runId}>
                {lastAck.duplicate ? "幂等命中" : "已受理"} {lastAck.runId.slice(-6)}
              </span>
            ) : null}
          </div>
        ) : null}
      </div>

      <div className="topbar-right">
        <span className="runtime-badges">
          {runtimes.map((runtime) => {
            const reason = runtime.status_reason ?? null;
            return (
              <span
                key={runtime.id}
                className={`runtime-badge runtime-${runtime.status}`}
                data-testid="runtime-badge"
                data-runtime-id={runtime.id}
                data-status={runtime.status}
                title={
                  `${runtime.name}：${RUNTIME_STATUS_LABELS[runtime.status]}` +
                  (reason ? `（${RUNTIME_REASON_LABELS[reason] ?? reason}）` : "")
                }
              >
                {runtime.name}·{RUNTIME_STATUS_LABELS[runtime.status]}
              </span>
            );
          })}
        </span>
        <StorageIndicator />
        <span
          className="pending-permission-badge"
          data-testid="pending-permission-badge"
          data-count={pendingCount}
          title="待审批计数（事件增量维护 + 面板校准）"
        >
          待审批：{pendingCount}
        </span>
        <button
          type="button"
          className="right-panel-toggle"
          data-testid="right-panel-toggle"
          aria-expanded={rightOpen}
          onClick={onToggleRight}
        >
          状态
        </button>
      </div>
    </header>
  );
}
