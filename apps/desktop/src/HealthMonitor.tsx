/**
 * 核心健康监控条（M2-07 DoD4/DoD5；D2 UI 健康）。
 *
 * - `unresponsive`：15s 无健康响应 → 「核心未响应」+ 重启入口（app_restart）；
 * - `degraded`：`storage_state=persist_degraded` → 只读横幅（M3-03/M3-06 消费同一状态）；
 * - `normal`：正常指示（E2E 断言锚点）；
 * - `loading`：首轮健康查询进行中。
 */
import { useCallback, useState } from "react";

import { requestAppRestart } from "./health";
import { describeIpcError } from "./startup";
import { useHealthPolling } from "./useHealthPolling";

export function HealthMonitor() {
  const { status, report, error, errorCode } = useHealthPolling();
  const [restartError, setRestartError] = useState<string | null>(null);
  const [restartPending, setRestartPending] = useState(false);

  const onRestart = useCallback(() => {
    setRestartPending(true);
    setRestartError(null);
    requestAppRestart()
      .catch((restartFailure: unknown) => {
        setRestartError(describeIpcError(restartFailure));
      })
      .finally(() => {
        setRestartPending(false);
      });
  }, []);

  if (status === "unresponsive") {
    return (
      <section
        className="health-unresponsive"
        data-testid="core-unresponsive"
        role="alert"
      >
        <p className="health-title">核心未响应</p>
        <p className="health-detail">
          连续 15s 未收到健康响应（UI 每 5s 轮询 health）。
          {error ? ` 最近错误：${error}` : ""}
        </p>
        <button
          type="button"
          data-testid="core-restart"
          onClick={onRestart}
          disabled={restartPending}
        >
          重启核心
        </button>
        {restartError ? (
          <p className="health-error" data-testid="core-restart-error">
            {restartError}
          </p>
        ) : null}
      </section>
    );
  }

  if (status === "degraded") {
    const trigger = report?.degrade_trigger ?? "unknown";
    return (
      <section
        className="health-degraded"
        data-testid="storage-degraded"
        role="alert"
      >
        <p className="health-title">存储降级（只读）</p>
        <p className="health-detail">
          触发源：{trigger}；写入与新任务已停止；运行中的任务已中断（可重试）。
          {report?.detail ? ` ${report.detail}` : ""}
        </p>
        {/* M3-06：恢复引导仅提供 app_restart（修复外部条件 + 重启核心 + 启动自检）；
            不提供任何「一键恢复/热恢复」按钮（D4/ADR-004）。 */}
        <p className="health-recovery-hint" data-testid="storage-degraded-hint">
          恢复方式：修复磁盘/目录/权限后重启应用（P0 无热恢复）。
        </p>
        <button
          type="button"
          data-testid="storage-degraded-restart"
          onClick={onRestart}
          disabled={restartPending}
        >
          重启应用
        </button>
        {restartError ? (
          <p className="health-error" data-testid="storage-degraded-restart-error">
            {restartError}
          </p>
        ) : null}
      </section>
    );
  }

  if (status === "normal") {
    return (
      <p className="health-normal" data-testid="health-normal">
        核心正常（storage_state=normal）
      </p>
    );
  }

  // M3-06：启动序列过渡窗口（ADR-007 增量 2）——后端注入前 `health` 返回
  // `core_not_ready`；按结构化错误码判别（不做文案匹配），完成后自动消失。
  if (status === "loading" && errorCode === "core_not_ready") {
    return (
      <section className="health-starting" data-testid="core-not-ready-banner" role="status">
        <p className="health-title">核心启动中…</p>
        <p className="health-detail">
          启动序列尚未完成（存储/管线注入前）；启动门命令（startup_*）仍可用。
        </p>
      </section>
    );
  }

  return (
    <p className="health-loading" data-testid="health-loading">
      正在获取核心健康状态…
    </p>
  );
}
