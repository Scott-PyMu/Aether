/**
 * 工作台顶栏（M3-03；UI-UX §2.5/Q8：合并 M3-02 状态条 + 运行/存储/待审批摘要）。
 *
 * - 左：应用名 + 当前会话标题 + 会话状态 chip（`session-status`）；
 * - 中：会话操作「⋯」菜单（运行状态 / 中断 / 暂停 / 导出 / 最近 ack；原状态条
 *   testid 保持不变，Q8「避免双状态条」；原型头部不常驻该状态条）；
 * - 右：运行时紧凑徽标组（`runtime-badge`，hover 显示 reason 中文释义）、存储状态
 *   （`storage-indicator`）、待审批计数（`pending-permission-badge`）、右栏抽屉开关
 *   （<1280px 断点由 CSS 控制显示，见 styles.css）。
 */
import { useEffect, useState } from "react";

import { useStorageHealth } from "./healthBus";
import {
  PAUSE_SEMANTICS_NOTICE,
  PAUSE_UNSUPPORTED_HINT,
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
  /** M5-05：单会话导出入口（提供时渲染；锚点 `session-export`）。 */
  onExportSession?: () => void;
  exportBusy?: boolean;
  exportResult?: "ok" | "failed" | null;
  /**
   * M7-03/ADR-017：暂停/恢复入口（锚点 `session-pause-toggle`；
   * `data-status` = 会话状态，`data-disabled` = 能力未声明/连接未就绪/无会话）。
   */
  pauseSupported?: boolean;
  pauseDisabled?: boolean;
  onTogglePause?: () => void;
  pauseBusy?: boolean;
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
  onExportSession,
  exportBusy = false,
  exportResult = null,
  pauseSupported = false,
  pauseDisabled = false,
  onTogglePause,
  pauseBusy = false,
}: TopBarProps) {
  const [actionsOpen, setActionsOpen] = useState(false);

  useEffect(() => {
    if (!actionsOpen) {
      return undefined;
    }
    const onDocumentClick = () => setActionsOpen(false);
    document.addEventListener("click", onDocumentClick);
    return () => document.removeEventListener("click", onDocumentClick);
  }, [actionsOpen]);

  return (
    <header className="workbench-topbar head" data-testid="topbar">
      <div className="topbar-left head-main">
        {session ? (
          <>
            <div className="session-actions">
              <button
                type="button"
                className="ttl topbar-session-title session-actions-toggle"
                data-testid="session-actions-toggle"
                aria-expanded={actionsOpen}
                aria-label="会话操作"
                title="会话操作（运行状态 / 中断 / 暂停 / 导出）"
                onClick={(event) => {
                  event.stopPropagation();
                  setActionsOpen((open) => !open);
                }}
              >
                <span className="session-actions-label">{session.title}</span>
              </button>
              <div
                className={actionsOpen ? "session-actions-menu show" : "session-actions-menu"}
                data-testid="session-actions-menu"
                onClick={(event) => event.stopPropagation()}
              >
                <div className="session-actions-head">
                  <span className="session-actions-title">会话操作</span>
                  <span
                    className="topbar-session-status"
                    data-testid="session-status"
                    data-status={session.status}
                  >
                    {SESSION_STATUS_LABELS[session.status]}
                  </span>
                </div>
                <div className="session-status-bar" data-testid="status-bar">
                  <span data-testid="run-status" data-run-id={runId ?? ""}>
                    {runStatusText}
                  </span>
                  <button
                    type="button"
                    className="head-btn"
                    data-testid="interrupt"
                    disabled={!interruptible}
                    onClick={onInterrupt}
                  >
                    中断
                  </button>
                  {/* M7-03/ADR-017：暂停/恢复（锚点 `session-pause-toggle`；能力未声明或
                      连接未就绪置灰 + tooltip `pause_unsupported` 文案）。 */}
                  {onTogglePause ? (
                    <button
                      type="button"
                      className="head-btn"
                      data-testid="session-pause-toggle"
                      data-session-id={session.id}
                      data-status={session.status}
                      data-disabled={String(pauseDisabled)}
                      disabled={pauseDisabled || pauseBusy}
                      title={
                        pauseSupported
                          ? session.status === "paused"
                            ? "恢复：重放被中断的输入（原生会话上下文保留，Mode R）"
                            : PAUSE_SEMANTICS_NOTICE
                          : PAUSE_UNSUPPORTED_HINT
                      }
                      onClick={onTogglePause}
                    >
                      {session.status === "paused" ? "恢复" : "暂停"}
                    </button>
                  ) : null}
                  {onExportSession ? (
                    <button
                      type="button"
                      className="head-btn"
                      data-testid="session-export"
                      data-result={exportResult ?? ""}
                      disabled={exportBusy}
                      title="导出该会话为 JSONL 归档（缺省写入数据目录 archives/）"
                      onClick={onExportSession}
                    >
                      导出
                    </button>
                  ) : null}
                  {lastAck ? (
                    <span className="last-ack" data-testid="last-ack" data-run-id={lastAck.runId}>
                      {lastAck.duplicate ? "幂等命中" : "已受理"} {lastAck.runId.slice(-6)}
                    </span>
                  ) : null}
                </div>
              </div>
            </div>
          </>
        ) : (
          <span className="ttl topbar-session-title muted">新 Agent 会话</span>
        )}
      </div>

      <div className="topbar-right head-actions">
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
      </div>
    </header>
  );
}
