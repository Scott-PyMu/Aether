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
          `诊断包已导出：${result.path}` +
          `（${result.bytes} 字节；日志 ${result.log_lines} 行；任务 dump ${result.task_dumps} 条；` +
          `脱敏守门 ${result.scanned_clean ? "0 命中" : "未通过"}）`,
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
          <h2>诊断导出</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            返回工作台
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
            ? `${capacityLevelLabel(capacityLevel)}（≥${capacity ? "5GB" : "阈值"}）：${capacityText}`
            : capacityLevel === "warn"
              ? `${capacityLevelLabel(capacityLevel)}：${capacityText}`
              : capacityText}
        </p>

        <section className="diagnostics-target-section">
          <label>
            导出目录（外部路径；经系统目录选择器选择）
            <input
              type="text"
              data-testid="diagnostics-target"
              value={target}
              readOnly
              placeholder="点击「选择目录…」"
            />
          </label>
          <div className="diagnostics-actions">
            <button
              type="button"
              data-testid="diagnostics-pick"
              disabled={busy}
              onClick={() => void pickTarget()}
            >
              选择目录…
            </button>
            <button
              type="button"
              data-testid="diagnostics-export"
              disabled={busy || target.trim().length === 0}
              onClick={() => void exportBundle()}
            >
              导出诊断包
            </button>
          </div>
          <p className="diagnostics-note">
            诊断包含版本 / 健康快照 / 安全级别 / 容量 / 库摘要 / 设置（脱敏）/
            运行期日志汇聚产物 / 任务 dump；核心将两轮密钥模式扫描（`sk-`/`eyJ`/PEM），
            任一次命中即拒绝写出（D10）。
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
