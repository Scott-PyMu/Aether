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
import { t } from "./i18n";

export interface HealthMonitorProps {
  /** M3-05：降级横幅「导出诊断」入口（打开 S-07 诊断导出页）。 */
  onOpenDiagnostics?: () => void;
}

export function HealthMonitor({ onOpenDiagnostics }: HealthMonitorProps = {}) {
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
        <p className="health-title">{t("health.core_not_responding")}</p>
        <p className="health-detail">
          {t("health.no_health_response_for_15s")}
          {error ? t("health.last_error", { p0: error }) : ""}
        </p>
        <button
          type="button"
          data-testid="core-restart"
          onClick={onRestart}
          disabled={restartPending}
        >
          {t("health.restart_core")}
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
        <p className="health-title">{t("health.storage_degraded_read_only")}</p>
        <p className="health-detail">
          {t("health.trigger")}{trigger}{t("health.writes_and_new_tasks_are")}
          {report?.detail ? ` ${report.detail}` : ""}
        </p>
        {/* M3-06：恢复引导仅提供 app_restart（修复外部条件 + 重启核心 + 启动自检）；
            不提供任何「一键恢复/热恢复」按钮（D4/ADR-004）。 */}
        <p className="health-recovery-hint" data-testid="storage-degraded-hint">
          {t("health.recovery_fix_the_disk_directory")}
        </p>
        <button
          type="button"
          data-testid="storage-degraded-restart"
          onClick={onRestart}
          disabled={restartPending}
        >
          {t("health.restart_app")}
        </button>
        {/* M3-05：降级期诊断导出入口（D4：读/备份/导出可用）。 */}
        {onOpenDiagnostics ? (
          <button
            type="button"
            data-testid="storage-degraded-diagnostics"
            onClick={onOpenDiagnostics}
          >
            {t("app.action.export_diagnostics")}
          </button>
        ) : null}
        {restartError ? (
          <p className="health-error" data-testid="storage-degraded-restart-error">
            {restartError}
          </p>
        ) : null}
      </section>
    );
  }

  if (status === "normal" && errorCode !== "core_not_ready") {
    return (
      <p className="health-normal" data-testid="health-normal">
        {t("health.core_healthy_storage_state_normal")}
      </p>
    );
  }

  // M3-06/M9-09：启动过渡窗口或 daemon 崩溃重启窗口——后端返回
  // `core_not_ready` 时即使上一轮状态为 normal 也显示核心横幅（保留窗口）。
  if (errorCode === "core_not_ready") {
    return (
      <section className="health-starting" data-testid="core-not-ready-banner" role="status">
        <p className="health-title">{t("health.core_starting")}</p>
        <p className="health-detail">
          {t("health.the_startup_sequence_has_not")}
        </p>
      </section>
    );
  }

  return (
    <p className="health-loading" data-testid="health-loading">
      {t("health.fetching_core_health")}
    </p>
  );
}
