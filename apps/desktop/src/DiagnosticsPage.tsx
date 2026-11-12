/**
 * 诊断导出页（M3-05；设计 D11/D13、ADR-007 增量，UI-UX S-07/§7.3）。
 *
 * - 覆盖层管理视图（`overlay-diagnostics` + `overlay-back`；不卸载工作台状态）；
 * - 容量状态（`diagnostics-capacity`：`data-level` ok/warn/critical；D13 2GB/5GB，
 *   阈值由核心参数化投影）；
 * - 选择外部目录（`diagnostics-pick`；复用 `startup_pick_target` 系统选择器）→
 *   `export_diagnostics`（`diagnostics-export`）→ 回执（`diagnostics-result`：
 *   `data-result=ok|failed`；成功展示产物路径 / 字节数 / 段清单 / 脱敏守门结果）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { BackupCapacity } from "./backup";
import { capacityLevelLabel, capacitySummary } from "./capacity";
import { diagnosticsIpc, type DiagnosticsIpc } from "./diagnostics";
import { describeIpcError, ipcErrorCode } from "./startup";
import { t } from "./i18n";

export interface DiagnosticsPageProps {
  /** IPC 契约（缺省 = 生产 Tauri 实现；测试注入替身）。 */
  ipc?: DiagnosticsIpc;
  /** 返回工作台（`overlay-back`）。 */
  onBack: () => void;
}

type ExportOutcome =
  | { result: "ok"; message: string }
  | { result: "failed"; message: string };

export function DiagnosticsPage({ ipc = diagnosticsIpc, onBack }: DiagnosticsPageProps) {
  const [capacity, setCapacity] = useState<BackupCapacity | null>(null);
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [outcome, setOutcome] = useState<ExportOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);

  const refreshCapacity = useCallback(async () => {
    try {
      setCapacity(await ipc.capacity());
    } catch {
      // 容量不可用时保持空态（诊断导出本身不依赖容量查询）。
      setCapacity(null);
    }
  }, [ipc]);

  useEffect(() => {
    void refreshCapacity();
  }, [refreshCapacity]);

  const pickTarget = useCallback(async () => {
    setError(null);
    setErrorCode(null);
    try {
      const picked = await ipc.pickTargetDir();
      if (picked === null) {
        return;
      }
      setTarget(picked);
      setOutcome(null);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [ipc]);

  const exportBundle = useCallback(async () => {
    setBusy(true);
    setError(null);
    setErrorCode(null);
    try {
      const result = await ipc.exportDiagnostics(target);
      setOutcome({
        result: "ok",
        message:
          t("app.diagnostics.diagnostics_bundle_exported", { p0: result.path }) +
          t("app.diagnostics.bytes_log_lines_task_dumps", { p0: result.bytes, p1: result.log_lines, p2: result.task_dumps }) +
          t("app.diagnostics.redaction_gate", { p0: result.scanned_clean ? t("app.diagnostics.0_hits") : t("app.diagnostics.failed") }),
      });
      await refreshCapacity();
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
      setOutcome({ result: "failed", message: describeIpcError(failure) });
    } finally {
      setBusy(false);
    }
  }, [ipc, refreshCapacity, target]);

  const capacityLevel = capacity?.level ?? "ok";
  const capacityText = useMemo(() => capacitySummary(capacity), [capacity]);

  return (
    <div className="overlay" data-testid="overlay-diagnostics">
      <section
        className="diagnostics-page"
        data-testid="diagnostics-page"
        role="dialog"
        aria-modal="true"
      >
        <header className="diagnostics-header">
          <h2>{t("app.common.diagnostics_export")}</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            {t("app.action.back_to_workbench")}
          </button>
        </header>

        <p
          className="capacity-status"
          data-testid="diagnostics-capacity"
          data-level={capacityLevel}
          data-total-bytes={capacity?.total_bytes ?? 0}
          role={capacityLevel === "ok" ? undefined : "alert"}
        >
          {capacityLevel === "critical"
            ? t("app.diagnostics.text", { p0: capacityLevelLabel(capacityLevel), p1: capacity ? "5GB" : t("app.diagnostics.threshold"), p2: capacityText })
            : capacityLevel === "warn"
              ? t("app.diagnostics.text_3", { p0: capacityLevelLabel(capacityLevel), p1: capacityText })
              : capacityText}
        </p>

        <section className="diagnostics-target-section">
          <label>
            {t("app.diagnostics.export_directory_external_path_chosen")}
            <input
              type="text"
              data-testid="diagnostics-target"
              value={target}
              readOnly
              placeholder={t("app.diagnostics.click_choose_directory")}
            />
          </label>
          <div className="diagnostics-actions">
            <button
              type="button"
              data-testid="diagnostics-pick"
              disabled={busy}
              onClick={() => void pickTarget()}
            >
              {t("app.action.choose_directory")}
            </button>
            <button
              type="button"
              data-testid="diagnostics-export"
              disabled={busy || target.trim().length === 0}
              onClick={() => void exportBundle()}
            >
              {t("app.action.export_diagnostics_bundle")}
            </button>
          </div>
          <p className="diagnostics-note">
            {t("app.diagnostics.the_bundle_includes_version_health")}
          </p>
        </section>

        {outcome ? (
          <p
            className="diagnostics-result"
            data-testid="diagnostics-result"
            data-result={outcome.result}
            role={outcome.result === "failed" ? "alert" : undefined}
          >
            {outcome.message}
          </p>
        ) : null}

        {error ? (
          <p
            className="diagnostics-error"
            data-testid="diagnostics-error"
            data-code={errorCode ?? ""}
            role="alert"
          >
            {error}
          </p>
        ) : null}
      </section>
    </div>
  );
}
