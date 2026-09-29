/**
 * 右栏诊断分区（M3-05；UI-UX §2.5/Q15：内联容量摘要 + 「导出诊断」入口）。
 *
 * - `diagnostics-entry-capacity`：容量等级（`data-level` ok/warn/critical；阈值由核心
 *   参数化投影，与 S-06/S-07 同一事实源）；
 * - `diagnostics-entry-open`：打开 S-07 诊断导出页（导出动作统一在页面内完成）。
 *
 * 仅在工作台提供 `onOpenDiagnostics` 时渲染（既有测试/嵌入场景 DOM 不变）。
 */
import { useEffect, useState } from "react";

import type { BackupCapacity } from "./backup";
import { capacityLevelLabel, capacitySummary } from "./capacity";
import { diagnosticsIpc, type DiagnosticsIpc } from "./diagnostics";

export interface DiagnosticsEntryProps {
  ipc?: DiagnosticsIpc;
  onOpen: () => void;
}

export function DiagnosticsEntry({ ipc = diagnosticsIpc, onOpen }: DiagnosticsEntryProps) {
  const [capacity, setCapacity] = useState<BackupCapacity | null>(null);

  useEffect(() => {
    let active = true;
    ipc
      .capacity()
      .then((value) => {
        if (active) {
          setCapacity(value);
        }
      })
      .catch(() => {
        if (active) {
          setCapacity(null);
        }
      });
    return () => {
      active = false;
    };
  }, [ipc]);

  const level = capacity?.level ?? "ok";
  return (
    <section className="diagnostics-entry" data-testid="right-panel-diagnostics">
      <h3>诊断</h3>
      <p
        className="diagnostics-entry-capacity"
        data-testid="diagnostics-entry-capacity"
        data-level={level}
        role={level === "ok" ? undefined : "alert"}
        title={capacitySummary(capacity)}
      >
        {capacity === null
          ? "容量：查询中…"
          : `${capacityLevelLabel(level)}：${capacitySummary(capacity)}`}
      </p>
      <button type="button" data-testid="diagnostics-entry-open" onClick={onOpen}>
        导出诊断
      </button>
    </section>
  );
}
