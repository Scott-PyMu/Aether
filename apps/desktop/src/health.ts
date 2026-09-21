/**
 * `health` IPC 接口（M2-07 DoD5；ADR-007 附录 A）。
 *
 * 与 Rust 侧 `HealthReport` 的 JSON 形状一致（snake_case）；T14（M3-01）生成
 * tauri-specta bindings 前，此文件是唯一前端契约来源。
 */
import { invoke } from "@tauri-apps/api/core";

/** UI 轮询周期（D2/ADR-007 A.3：每 5s 调 `health`）。 */
export const HEALTH_POLL_INTERVAL_MS = 5_000;
/** 无响应判定窗口（D2/ADR-007 A.3：15s 无响应 → 「核心未响应」+ 重启入口）。 */
export const HEALTH_TIMEOUT_MS = 15_000;

export type StorageState = "normal" | "persist_degraded";

/** `health.runtimes` 条目（`null` = 监督器未接线；`[]` = 已接线无 runtime）。 */
export interface RuntimeSummary {
  id: string;
  status: "cold" | "starting" | "ready" | "degraded" | "disabled";
  status_reason?: string;
}

export interface HealthReport {
  storage_state: StorageState;
  write_queue_depth: number;
  runtimes: RuntimeSummary[] | null;
  ts: number;
  degrade_trigger?: string;
  degraded_since_ms?: number;
  detail?: string;
}

export async function fetchHealth(): Promise<HealthReport> {
  return invoke<HealthReport>("health");
}

/**
 * 重启入口（M3-06 实现完整关闭序列；M2-07 提供入口与错误呈现）。
 * 复用 ADR-004 `app_restart` 契约：显式 `confirm:true`。
 */
export async function requestAppRestart(): Promise<void> {
  await invoke("app_restart", { payload: { confirm: true } });
}
