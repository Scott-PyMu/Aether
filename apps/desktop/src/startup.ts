/**
 * 启动门 IPC 接口（M1-06 / A4）。
 *
 * 与 Rust 侧 `StartupSnapshot` / `DetectionReport` 的 JSON 形状一致（snake_case）；
 * T14（M3-01）生成 tauri-specta bindings 前，此文件是唯一前端契约来源。
 */
import { invoke } from "@tauri-apps/api/core";

export type StartupPhase = "ready" | "blocked_sync_dir" | "blocked_error";

export type DataDirSource = "env_override" | "pointer" | "default" | "migrated";

export interface DetectionCheck {
  id: string;
  label: string;
  hit: boolean;
  detail: string;
  precision: "exact" | "path_prefix";
}

export interface DetectionReport {
  platform: "windows" | "macos" | "other";
  candidate: string;
  resolved: string;
  verdict: "allow" | "reject";
  checks: DetectionCheck[];
  reasons: string[];
  note?: string;
}

export interface MigrationEntry {
  relative: string;
  sha256: string;
  bytes: number;
}

export interface MigrationOutcome {
  source: string;
  target: string;
  entries: MigrationEntry[];
  total_bytes: number;
}

export type MigrationPhase = "copying" | "verified" | "pointer_written" | "done";

/** 未完成迁移（指针写入失败窗口）：UI 提供「完成迁移」入口。 */
export interface PendingMigration {
  migration_id: string;
  target: string;
  phase: MigrationPhase;
  started_at: number;
}

export interface StartupSnapshot {
  phase: StartupPhase;
  data_dir: string;
  data_dir_source: DataDirSource;
  detection?: DetectionReport;
  message?: string;
  migration?: MigrationOutcome;
  pending_migration?: PendingMigration;
}

export async function fetchStartup(): Promise<StartupSnapshot> {
  return invoke<StartupSnapshot>("startup_get");
}

export async function migrateDataDir(
  targetDir: string,
): Promise<StartupSnapshot> {
  return invoke<StartupSnapshot>("startup_migrate", {
    payload: { target_dir: targetDir },
  });
}

export async function pickMigrationTarget(): Promise<string | null> {
  const result = await invoke<{ target_dir: string | null }>(
    "startup_pick_target",
  );
  return result.target_dir ?? null;
}

export async function exitApp(): Promise<void> {
  await invoke("app_exit");
}

/** 将 IPC 结构化错误转为可展示文本（`message` 优先，兜底 String(error)）。 */
export function describeIpcError(error: unknown): string {
  if (typeof error === "object" && error !== null) {
    const record = error as Record<string, unknown>;
    if (typeof record.message === "string" && record.message.length > 0) {
      return record.message;
    }
  }
  return String(error);
}
