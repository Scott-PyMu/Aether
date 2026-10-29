/**
 * `health` IPC 接口（M2-07 DoD5；ADR-007 附录 A）。
 *
 * 与 Rust 侧 `HealthReport` 的 JSON 形状一致（snake_case）；M9-08 起经
 * `@aether/protocol` typed client 调用。
 */
import { client } from "@aether/protocol";

/** UI 轮询周期（D2/ADR-007 A.3：每 5s 调 `health`）。 */
export const HEALTH_POLL_INTERVAL_MS = 5_000;
/** 无响应判定窗口（D2/ADR-007 A.3：15s 无响应 → 「核心未响应」+ 重启入口）。 */
export const HEALTH_TIMEOUT_MS = 15_000;
/**
 * 写队列 L2 阈值（D8/ADR-004 冻结值 4096；`storage-backpressure-notice` 展示用）。
 *
 * 与核心侧 `EventPipeline::admission()` 的 L2 口径一致：写队列 >4096 时拒绝新 run
 * （`storage_backpressure`），队列回落 ≤1024 自动解除（仅队列维度，与
 * `persist_degraded` 的修复+重启路径区分）。
 */
export const STORAGE_L2_THRESHOLD = 4_096;

export type StorageState = "normal" | "persist_degraded";

/**
 * 健康轮询状态（M2-07；M3-06 经 `healthBus` 共享给工作台）。
 *
 * - `loading`：首轮查询进行中（含 ADR-007 过渡窗口 `core_not_ready`）；
 * - `normal`：`storage_state=normal`；
 * - `degraded`：`storage_state=persist_degraded`（只读；发送入口禁用，M3-06）；
 * - `unresponsive`：15s 无响应（「核心未响应」+ 重启入口）。
 */
export type HealthStatus = "loading" | "normal" | "degraded" | "unresponsive";

/** `health.runtimes` 条目（`null` = 监督器未接线；`[]` = 已接线无 runtime）。 */
export interface RuntimeSummary {
  id: string;
  status: "cold" | "starting" | "ready" | "degraded" | "disabled";
  status_reason?: string;
}

/**
 * 并发闸门快照（M7-02/ADR-022 附录 A-5：`health.concurrency_gate`，UI 轮询 5s；
 * 未接线省略）。`limit = clamp(floor(free_ram_gb / 1.5), 2, 10)`；`active` = 全局
 * running run 数；`queued` = 全局 FIFO 长度。
 */
export interface ConcurrencyGate {
  limit: number;
  active: number;
  queued: number;
}

export interface HealthReport {
  storage_state: StorageState;
  write_queue_depth: number;
  runtimes: RuntimeSummary[] | null;
  /** M7-02/ADR-022：并发闸门快照（未接线省略）。 */
  concurrency_gate?: ConcurrencyGate;
  ts: number;
  degrade_trigger?: string;
  degraded_since_ms?: number;
  detail?: string;
}

export async function fetchHealth(): Promise<HealthReport> {
  return client.health<HealthReport>();
}

/**
 * 重启入口（M3-06 实现完整关闭序列；M2-07 提供入口与错误呈现）。
 * 复用 ADR-004 `app_restart` 契约：显式 `confirm:true`。
 */
export async function requestAppRestart(): Promise<void> {
  await client.appRestart({ confirm: true });
}
