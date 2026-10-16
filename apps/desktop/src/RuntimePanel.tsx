/**
 * 运行时状态面板（M3-03；设计 D5，UI-UX S-04/§3.2 状态呈现表；M7-01 启停开关）。
 *
 * - 每运行时一行：状态徽标 + `status_reason` 中文释义 + 故障恢复入口；
 * - `disabled + start_failed` → `runtime_retry`（M1-10）；
 * - `disabled`（其余 reason，含 `user_disabled`）→ `runtime_enable`（M1-10/ADR-015）；
 * - `untrusted` / `version_mismatch` 禁止直接启用，仅展示修复路径（D5/ADR-008）；
 * - `degraded + storage_backpressure` 为自动解除隔离，不提供手动入口（D8）；
 * - M7-01/ADR-015（决策 3/6）：`runtime-toggle` 启停开关（`data-runtime-id`/
 *   `data-enabled`/`data-status`）；禁用存在在途会话 → `runtime-disable-confirm`
 *   二次确认（`data-count`），确认后携 `confirm_terminate` 重试；
 *   `disabled + user_disabled` 展示「已禁用·手动」边界说明（附录 E-2）。
 */
import { useCallback, useState } from "react";

import { RUNTIME_OUTCOME_LABELS, type PermissionIpc } from "./permission";
import { RUNTIME_REASON_LABELS, RUNTIME_STATUS_LABELS, type RuntimeInfo } from "./session";
import { describeIpcError, ipcErrorCode } from "./startup";

export interface RuntimePanelProps {
  runtimes: RuntimeInfo[];
  ipc: PermissionIpc;
  /** 控制命令成功后刷新注册表（`runtimes_list`）。 */
  onRefresh: () => void | Promise<void>;
}

interface RuntimeActions {
  retry: boolean;
  enable: boolean;
  /** 禁止直接启用的修复说明（untrusted / version_mismatch）。 */
  remedy: string | null;
}

/** 状态 → 可用操作（与 UI-UX §3.2 表逐行对应）。 */
export function runtimeActions(runtime: RuntimeInfo): RuntimeActions {
  if (runtime.status !== "disabled") {
    return { retry: false, enable: false, remedy: null };
  }
  switch (runtime.status_reason) {
    case "start_failed":
      return { retry: true, enable: false, remedy: null };
    case "untrusted":
      return {
        retry: false,
        enable: false,
        remedy: "P0 仅支持官方适配器；不可直接启用（D5 §2.1）。",
      };
    case "version_mismatch":
      return {
        retry: false,
        enable: false,
        remedy: "运行时版本不匹配：需升级适配器/应用后重启；不可直接启用（ADR-002/008）。",
      };
    default:
      // handshake_timeout / crash_loop / user_disabled / 无 reason：可重新启用（D5/ADR-015）。
      return { retry: false, enable: true, remedy: null };
  }
}

/** `user_disabled` 边界说明（ADR-015 附录 E-2：会话与数据保留，重新启用后可继续/重放）。 */
export const RUNTIME_USER_DISABLED_HINT =
  "已由用户禁用；会话与数据保留，重新启用后可继续或重放";

interface DisableConfirm {
  runtimeId: string;
  count: number;
}

/** 从 `runtime_busy` 错误文案提取在途计数（服务端竞态兜底；列表字段为主来源）。 */
function busyCountFromError(failure: unknown): number {
  const message = describeIpcError(failure);
  const matched = /(\d+)\s*个在途/.exec(message);
  const raw = matched?.[1];
  if (!raw) {
    return 0;
  }
  const parsed = Number.parseInt(raw, 10);
  return Number.isNaN(parsed) ? 0 : parsed;
}

export function RuntimePanel({ runtimes, ipc, onRefresh }: RuntimePanelProps) {
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 结构化错误码（UI-UX §7.3 通用行：错误元素补 `data-code`）。 */
  const [errorCode, setErrorCode] = useState<string | null>(null);
  /** M7-01：禁用二次确认（在途会话 >0；锚点 `runtime-disable-confirm`）。 */
  const [confirm, setConfirm] = useState<DisableConfirm | null>(null);

  const reportSuccess = useCallback((result: {
    runtime_id: string;
    outcome: string;
    status_reason?: string | null;
    detail?: string | null;
  }) => {
    const outcome = RUNTIME_OUTCOME_LABELS[result.outcome] ?? result.outcome;
    const reason = result.status_reason
      ? `（${RUNTIME_REASON_LABELS[result.status_reason] ?? result.status_reason}）`
      : "";
    setNotice(
      `${result.runtime_id}：${outcome}${reason}${result.detail ? ` - ${result.detail}` : ""}`,
    );
  }, []);

  const onControl = useCallback(
    async (runtimeId: string, mode: "retry" | "enable") => {
      setBusyId(runtimeId);
      setNotice(null);
      setError(null);
      setErrorCode(null);
      try {
        const result =
          mode === "retry"
            ? await ipc.retryRuntime(runtimeId)
            : await ipc.enableRuntime(runtimeId);
        reportSuccess(result);
        await onRefresh();
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusyId(null);
      }
    },
    [ipc, onRefresh, reportSuccess],
  );

  const onDisable = useCallback(
    async (runtime: RuntimeInfo, confirmTerminate: boolean) => {
      setBusyId(runtime.id);
      setNotice(null);
      setError(null);
      setErrorCode(null);
      try {
        const result = await ipc.disableRuntime(runtime.id, confirmTerminate);
        reportSuccess(result);
        setConfirm(null);
        await onRefresh();
      } catch (failure) {
        const code = ipcErrorCode(failure);
        if (code === "runtime_busy" && !confirmTerminate) {
          // 竞态兜底：计数以错误文案解析为准（列表字段为主来源）。
          setConfirm({
            runtimeId: runtime.id,
            count: busyCountFromError(failure) || (runtime.active_sessions ?? 0),
          });
          return;
        }
        setError(describeIpcError(failure));
        setErrorCode(code);
      } finally {
        setBusyId(null);
      }
    },
    [ipc, onRefresh, reportSuccess],
  );

  /** 开关点击：`disabled` 且偏好为关 → 启用；运行中且偏好为开 → 禁用（在途先确认）。 */
  const onToggle = useCallback(
    (runtime: RuntimeInfo) => {
      if (runtime.status === "disabled") {
        if (!runtime.enabled) {
          void onControl(runtime.id, "enable");
        }
        return;
      }
      if (runtime.enabled) {
        const active = runtime.active_sessions ?? 0;
        if (active > 0) {
          setConfirm({ runtimeId: runtime.id, count: active });
          return;
        }
        void onDisable(runtime, false);
      }
    },
    [onControl, onDisable],
  );

  const confirmRuntime = confirm
    ? runtimes.find((runtime) => runtime.id === confirm.runtimeId) ?? null
    : null;

  return (
    <section className="runtime-panel" data-testid="runtime-panel">
      <h2 className="workbench-title">运行时</h2>
      {runtimes.length === 0 ? (
        <p className="runtime-panel-empty" data-testid="runtime-panel-empty">
          无已注册运行时
        </p>
      ) : null}
      <ul className="runtime-panel-list">
        {runtimes.map((runtime) => {
          const actions = runtimeActions(runtime);
          const reason = runtime.status_reason ?? null;
          const toggleLocked =
            busyId !== null || (runtime.status === "disabled" && runtime.enabled);
          return (
            <li
              key={runtime.id}
              className="runtime-panel-item"
              data-testid="runtime-panel-item"
              data-runtime-id={runtime.id}
              data-status={runtime.status}
              data-reason={reason ?? ""}
            >
              <div className="runtime-panel-head">
                <span className="runtime-panel-name">{runtime.name}</span>
                <span className="runtime-panel-status">
                  {RUNTIME_STATUS_LABELS[runtime.status]}
                </span>
                <button
                  type="button"
                  className={"runtime-toggle" + (runtime.enabled ? " on" : "")}
                  data-testid="runtime-toggle"
                  data-runtime-id={runtime.id}
                  data-enabled={String(runtime.enabled)}
                  data-status={runtime.status}
                  role="switch"
                  aria-checked={runtime.enabled}
                  aria-label={`${runtime.name} 启用开关`}
                  title={
                    runtime.status === "disabled" && runtime.enabled
                      ? "运行时不处于可禁用状态；请使用下方恢复操作"
                      : undefined
                  }
                  disabled={toggleLocked}
                  onClick={() => onToggle(runtime)}
                >
                  {runtime.enabled ? "已启用" : "已停用"}
                </button>
              </div>
              {reason ? (
                <p className="runtime-panel-reason">
                  {RUNTIME_REASON_LABELS[reason] ?? reason}
                </p>
              ) : null}
              {reason === "user_disabled" ? (
                <p className="runtime-panel-boundary" data-testid="runtime-user-disabled-hint">
                  {RUNTIME_USER_DISABLED_HINT}
                </p>
              ) : null}
              {runtime.capabilities.length > 0 ? (
                <p className="runtime-panel-capabilities">
                  {runtime.capabilities.join(" · ")}
                </p>
              ) : null}
              <div className="runtime-panel-actions">
                {actions.retry ? (
                  <button
                    type="button"
                    data-testid="runtime-retry"
                    disabled={busyId !== null}
                    onClick={() => void onControl(runtime.id, "retry")}
                  >
                    重试
                  </button>
                ) : null}
                {actions.enable ? (
                  <button
                    type="button"
                    data-testid="runtime-enable"
                    disabled={busyId !== null}
                    onClick={() => void onControl(runtime.id, "enable")}
                  >
                    重新启用
                  </button>
                ) : null}
              </div>
              {actions.remedy ? (
                <p className="runtime-panel-remedy" data-testid="runtime-remedy">
                  {actions.remedy}
                </p>
              ) : null}
            </li>
          );
        })}
      </ul>
      {confirm ? (
        <div
          className="runtime-disable-confirm"
          data-testid="runtime-disable-confirm"
          data-count={confirm.count}
          role="alertdialog"
          aria-label="禁用运行时确认"
        >
          <p className="runtime-disable-confirm-text">
            {`运行时有 ${confirm.count} 个在途会话；禁用将终止在途任务（会话与数据保留）。确认继续？`}
          </p>
          <div className="runtime-disable-confirm-actions">
            <button
              type="button"
              data-testid="runtime-disable-confirm-accept"
              disabled={busyId !== null || confirmRuntime === null}
              onClick={() => {
                if (confirmRuntime) {
                  void onDisable(confirmRuntime, true);
                }
              }}
            >
              确认禁用
            </button>
            <button
              type="button"
              data-testid="runtime-disable-confirm-cancel"
              onClick={() => setConfirm(null)}
            >
              取消
            </button>
          </div>
        </div>
      ) : null}
      {notice ? (
        <p className="runtime-panel-notice" data-testid="runtime-control-notice" role="status">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          className="runtime-panel-error"
          data-testid="runtime-control-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {error}
        </p>
      ) : null}
    </section>
  );
}
