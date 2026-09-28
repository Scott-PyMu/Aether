/**
 * 权限待办面板（M3-03；设计 D9，UI-UX S-03/§4.3）。
 *
 * - 非模态审批卡（Q3 裁定：不夺焦点；`role="alertdialog"` + `aria-modal="false"`）；
 * - 原文 target 与规范化结果并排对照（D9 评审 #10：防视觉欺骗）；
 * - 同一会话同时最多 1 张激活卡，其余排队展示（D9；服务层允许并存，UI 排队口径）；
 * - 300s 倒计时提示，超时后以 `permission.resolved(deny)` 事件收口为「已超时自动拒绝」；
 * - 清单全量校准（`permissions_pending`）+ 事件增量维护，不新增轮询（UI-UX §2.5）。
 */
import type { AetherEvent } from "@aether/protocol";
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  APPROVAL_TIMEOUT_MS,
  type PermissionDecision,
  type PermissionIpc,
  type PendingPermission,
} from "./permission";
import { useStorageHealth } from "./healthBus";
import { projectPermissions, targetsEquivalent } from "./permissionProjection";
import { describeIpcError, ipcErrorCode } from "./startup";

/** 已决议卡片的保留展示数量（弱化只读态；UI-UX §4.3）。 */
export const RESOLVED_HISTORY_LIMIT = 3;

export interface PermissionPanelProps {
  ipc: PermissionIpc;
  /** 当前激活会话（审批卡仅展示该会话；面板计数为全局）。 */
  sessionId: string | null;
  /** 当前会话事件流（`permission.requested` / `permission.resolved` 增量来源）。 */
  events: AetherEvent[];
  /** 待审批计数（全局面板徽标联动；TopBar `pending-permission-badge`）。 */
  onPendingCount?: (count: number) => void;
}

function remainingMs(requestedAt: number, timeoutMs: number, now: number): number {
  const timeout = timeoutMs > 0 ? timeoutMs : APPROVAL_TIMEOUT_MS;
  return Math.max(0, requestedAt + timeout - now);
}

export function PermissionPanel({
  ipc,
  sessionId,
  events,
  onPendingCount,
}: PermissionPanelProps) {
  const [pending, setPending] = useState<PendingPermission[]>([]);
  const [error, setError] = useState<string | null>(null);
  /** 结构化错误码（UI-UX §7.3 通用行：错误元素补 `data-code`）。 */
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [resolvingId, setResolvingId] = useState<string | null>(null);
  const [clockTick, setClockTick] = useState(0);
  const health = useStorageHealth();
  /** 核心就绪（非启动过渡窗口）——过渡窗口的 `core_not_ready` 失败在就绪后重取。 */
  const coreReady = health.status !== "loading";

  const permissionEventCount = useMemo(
    () =>
      events.filter(
        (event) =>
          event.type === "permission.requested" || event.type === "permission.resolved",
      ).length,
    [events],
  );

  const refresh = useCallback(async () => {
    try {
      const list = await ipc.pendingPermissions();
      setPending(list);
      setError(null);
      setErrorCode(null);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [ipc]);

  // 清单校准：打开面板/会话切换/权限事件到达/核心就绪（增量维护 + 全量校准，
  // 不新增轮询：核心就绪信号复用既有 health 轮询结果）。
  useEffect(() => {
    void refresh();
  }, [refresh, sessionId, permissionEventCount, coreReady]);

  useEffect(() => {
    onPendingCount?.(pending.length);
  }, [onPendingCount, pending.length]);

  const projected = useMemo(
    () => projectPermissions(events, pending),
    [events, pending],
  );
  const sessionItems = useMemo(
    () =>
      projected.filter(
        (item) => item.sessionId === sessionId || item.sessionId === null,
      ),
    [projected, sessionId],
  );
  const queue = sessionItems.filter((item) => item.status === "pending");
  const active = queue[0] ?? null;
  const resolved = sessionItems
    .filter((item) => item.status !== "pending")
    .slice(-RESOLVED_HISTORY_LIMIT)
    .reverse();

  // 倒计时（仅存在待审批时启动 1s 心跳；无待审批不产生定时器）。
  useEffect(() => {
    if (queue.length === 0) {
      return undefined;
    }
    const timer = window.setInterval(() => {
      setClockTick((tick) => tick + 1);
    }, 1000);
    return () => {
      window.clearInterval(timer);
    };
  }, [queue.length]);
  const now = useMemo(() => Date.now(), [clockTick]);

  const onResolve = useCallback(
    async (requestId: string, decision: PermissionDecision) => {
      setResolvingId(requestId);
      setError(null);
      setErrorCode(null);
      try {
        await ipc.resolvePermission(requestId, decision);
        await refresh();
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
        await refresh();
      } finally {
        setResolvingId(null);
      }
    },
    [ipc, refresh],
  );

  const busy = resolvingId !== null;
  const equal =
    active === null
      ? null
      : targetsEquivalent(active.target, active.canonicalTarget);
  const remaining = active === null ? 0 : remainingMs(active.requestedAt, active.timeoutMs, now);
  const expired = active !== null && remaining === 0;

  return (
    <section className="permission-panel" data-testid="permission-panel">
      <h2 className="workbench-title">
        权限待办
        <span
          className="permission-queue-count"
          data-testid="permission-queue-count"
          data-count={queue.length}
        >
          {queue.length}
        </span>
      </h2>

      {active === null && resolved.length === 0 ? (
        <p className="permission-empty" data-testid="permission-empty">
          {sessionId === null ? "请选择会话" : "暂无待审批"}
        </p>
      ) : null}

      {active !== null ? (
        <article
          className="permission-card"
          data-testid="permission-card"
          data-status="pending"
          data-request-id={active.requestId}
          role="alertdialog"
          aria-modal="false"
          aria-labelledby="permission-card-title"
        >
          <h3 className="permission-card-title" id="permission-card-title">
            权限请求
          </h3>
          <p className="permission-action">
            {active.resource}:{active.action}
          </p>
          <dl className="permission-targets">
            <dt>原始请求</dt>
            <dd data-testid="permission-target-raw">
              {active.target ?? "（无目标）"}
            </dd>
            <dt>规范化结果</dt>
            <dd
              data-testid="permission-target-canonical"
              data-equal={equal === null ? undefined : String(equal)}
            >
              {active.canonicalTarget ?? "（不可用）"}
              {equal === false ? "（与原文不一致，请核对）" : null}
              {equal === true ? "（规范化后一致）" : null}
            </dd>
          </dl>
          <p
            className="permission-timeout-note"
            data-testid="permission-timeout-note"
            data-remaining-ms={remaining}
            data-expired={String(expired)}
          >
            {expired
              ? "已超时，等待核心自动拒绝（300s）"
              : `剩余 ${Math.ceil(remaining / 1000)}s 后自动拒绝`}
          </p>
          <div className="permission-actions">
            <button
              type="button"
              data-testid="permission-allow-once"
              disabled={busy}
              onClick={() => void onResolve(active.requestId, "once")}
            >
              仅本次允许
            </button>
            <button
              type="button"
              data-testid="permission-allow-session"
              disabled={busy}
              onClick={() => void onResolve(active.requestId, "session")}
            >
              本会话允许
            </button>
            <button
              type="button"
              data-testid="permission-deny"
              disabled={busy}
              onClick={() => void onResolve(active.requestId, "deny")}
            >
              拒绝
            </button>
          </div>
          {queue.length > 1 ? (
            <p className="permission-queue-note" data-testid="permission-queue-note">
              还有 {queue.length - 1} 条排队
            </p>
          ) : null}
          {/* D9 边界声明（C6）：不得表述为「已隔离适配器内部行为」。 */}
          <p className="permission-boundary">
            权限门仅约束经线协议上报的工具调用；适配器进程内行为不受此门约束。
          </p>
        </article>
      ) : null}

      {resolved.length > 0 ? (
        <ul className="permission-resolved-list">
          {resolved.map((item) => (
            <li
              key={item.requestId}
              className="permission-card resolved"
              data-testid="permission-card"
              data-status={item.status}
              data-request-id={item.requestId}
            >
              <span>{item.resource}:{item.action}</span>
              <span className="permission-resolved-result">
                {item.status === "timeout"
                  ? "已超时自动拒绝（300s）"
                  : item.decision === "allow"
                    ? item.scope === "session"
                      ? "已允许（本会话）"
                      : "已允许（本次）"
                    : "已拒绝"}
              </span>
            </li>
          ))}
        </ul>
      ) : null}

      {error ? (
        <p
          className="permission-error"
          data-testid="permission-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {error}
        </p>
      ) : null}
    </section>
  );
}
