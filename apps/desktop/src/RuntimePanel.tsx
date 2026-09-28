/**
 * 运行时状态面板（M3-03；设计 D5，UI-UX S-04/§3.2 状态呈现表）。
 *
 * - 每运行时一行：状态徽标 + `status_reason` 中文释义 + 故障恢复入口；
 * - `disabled + start_failed` → `runtime_retry`（M1-10）；
 * - `disabled`（其余 reason）→ `runtime_enable`（M1-10）；
 * - `untrusted` / `version_mismatch` 禁止直接启用，仅展示修复路径（D5/ADR-008）；
 * - `degraded + storage_backpressure` 为自动解除隔离，不提供手动入口（D8）。
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
      // handshake_timeout / crash_loop / 无 reason：修复后可重新启用（D5）。
      return { retry: false, enable: true, remedy: null };
  }
}

export function RuntimePanel({ runtimes, ipc, onRefresh }: RuntimePanelProps) {
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 结构化错误码（UI-UX §7.3 通用行：错误元素补 `data-code`）。 */
  const [errorCode, setErrorCode] = useState<string | null>(null);

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
        const outcome = RUNTIME_OUTCOME_LABELS[result.outcome] ?? result.outcome;
        const reason = result.status_reason
          ? `（${RUNTIME_REASON_LABELS[result.status_reason] ?? result.status_reason}）`
          : "";
        setNotice(`${runtimeId}：${outcome}${reason}${result.detail ? ` - ${result.detail}` : ""}`);
        await onRefresh();
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusyId(null);
      }
    },
    [ipc, onRefresh],
  );

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
              </div>
              {reason ? (
                <p className="runtime-panel-reason">
                  {RUNTIME_REASON_LABELS[reason] ?? reason}
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
