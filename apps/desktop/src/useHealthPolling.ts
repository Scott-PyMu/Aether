/**
 * UI 健康轮询（M2-07 DoD5；D2 / ADR-007 附录 A.3）。
 *
 * 口径：
 * - 每 {@link HEALTH_POLL_INTERVAL_MS}（5s）调用一次 `health`；
 * - 距离最近一次成功响应（从未成功则以挂载时刻计）满 {@link HEALTH_TIMEOUT_MS}
 *   （15s）→ `unresponsive`（UI 显示「核心未响应」+ 重启入口）；
 * - 走查中一次成功即恢复 `normal` / `persist_degraded`（降级态由 `storage_state` 判定）；
 * - 调用挂起不叠加并发（`inFlight` 守卫），15s 由超时守卫独立判定。
 */
import { useEffect, useState } from "react";

import {
  fetchHealth,
  HEALTH_POLL_INTERVAL_MS,
  HEALTH_TIMEOUT_MS,
  type HealthReport,
} from "./health";
import { describeIpcError } from "./startup";

export type HealthStatus = "loading" | "normal" | "degraded" | "unresponsive";

export interface HealthState {
  status: HealthStatus;
  report: HealthReport | null;
  /** 最近一次失败原因（诊断展示；成功即清空）。 */
  error: string | null;
}

export interface HealthPollingOptions {
  pollIntervalMs?: number;
  timeoutMs?: number;
  /** 时间源（测试注入；默认 `Date.now`）。 */
  now?: () => number;
  /** 健康查询（测试注入；默认 IPC `health`）。 */
  fetchHealth?: () => Promise<HealthReport>;
}

export function useHealthPolling(options: HealthPollingOptions = {}): HealthState {
  const pollIntervalMs = options.pollIntervalMs ?? HEALTH_POLL_INTERVAL_MS;
  const timeoutMs = options.timeoutMs ?? HEALTH_TIMEOUT_MS;
  const [state, setState] = useState<HealthState>({
    status: "loading",
    report: null,
    error: null,
  });

  useEffect(() => {
    const now = options.now ?? Date.now;
    const query = options.fetchHealth ?? fetchHealth;
    let active = true;
    let inFlight = false;
    let lastOkAt: number | null = null;
    const startedAt = now();

    const applyReport = (report: HealthReport) => {
      lastOkAt = now();
      setState({
        status: report.storage_state === "persist_degraded" ? "degraded" : "normal",
        report,
        error: null,
      });
    };

    const poll = () => {
      if (inFlight) {
        return;
      }
      inFlight = true;
      query()
        .then((report) => {
          if (!active) {
            return;
          }
          applyReport(report);
        })
        .catch((error: unknown) => {
          if (!active) {
            return;
          }
          setState((previous) => ({
            ...previous,
            error: describeIpcError(error),
          }));
        })
        .finally(() => {
          inFlight = false;
        });
    };

    // 超时守卫独立于调用完成：挂起/失败混合场景均在 15s 判定。
    const guard = setInterval(
      () => {
        const since = lastOkAt ?? startedAt;
        if (now() - since >= timeoutMs) {
          setState((previous) =>
            previous.status === "unresponsive"
              ? previous
              : { ...previous, status: "unresponsive" },
          );
        }
      },
      Math.min(1_000, pollIntervalMs),
    );

    poll();
    const timer = setInterval(poll, pollIntervalMs);
    return () => {
      active = false;
      clearInterval(timer);
      clearInterval(guard);
    };
    // 轮询参数在挂载时固定（D2 常量；测试注入值同口径）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return state;
}
