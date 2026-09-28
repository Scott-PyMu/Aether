/**
 * 存储健康共享状态（M3-06）：`useHealthPolling`（HealthMonitor 持有）发布，
 * 工作台（发送入口禁用/中断提示）与降级横幅订阅同一状态——**全应用只轮询一次**。
 *
 * 实现为最小外部存储 + `useSyncExternalStore`（无额外依赖）；测试可经
 * `publishHealthState` 注入任意状态（注入 + E2E 口径）。
 */
import { useSyncExternalStore } from "react";

import type { HealthReport, HealthStatus } from "./health";

export interface HealthBusState {
  status: HealthStatus;
  report: HealthReport | null;
  error: string | null;
}

const INITIAL: HealthBusState = { status: "loading", report: null, error: null };

let current: HealthBusState = INITIAL;
const listeners = new Set<() => void>();

/** 发布健康状态（`useHealthPolling` 每次状态变化调用；测试可直接注入）。 */
export function publishHealthState(next: HealthBusState): void {
  current = next;
  for (const listener of [...listeners]) {
    listener();
  }
}

/** 当前快照（`useSyncExternalStore` 用；引用稳定，仅在发布时更换）。 */
export function healthBusSnapshot(): HealthBusState {
  return current;
}

export function subscribeHealthBus(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** 订阅存储健康状态（工作台降级联动）。 */
export function useStorageHealth(): HealthBusState {
  return useSyncExternalStore(subscribeHealthBus, healthBusSnapshot, healthBusSnapshot);
}

/** 测试隔离：复位到初始状态并清空订阅。 */
export function resetHealthBusForTests(): void {
  current = INITIAL;
  listeners.clear();
}
