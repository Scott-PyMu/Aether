/**
 * 备份与恢复 IPC 契约（M3-04；设计 D13、ADR-003/ADR-004，UI-UX S-06）。
 *
 * 与 Rust 侧 `backup_control.rs` 的 JSON 形状一致（snake_case）：
 * - `backup_list` → 内部备份清单（`backups` 表，最新在前）+ 容量状态（2GB/5GB）；
 * - `backup_create` → 手动备份（缺省 `backups/`；`target_dir` = 外部目录，D13）；
 * - `backup_restore` → 候选校验并登记恢复请求（`restart_required`：应用重启后由启动
 *   序列在无写者窗口执行 D13 七步 3–6；恢复成功则应用重启）。
 * - 外部目录选择复用 `startup_pick_target`（M1-06 系统目录选择器抽象；不新增命令）。
 *
 * 本文件是备份命令面的类型化封装（可注入替身；生产 = Tauri invoke）。
 */
import { invoke } from "@tauri-apps/api/core";

/** 备份台账记录（`backups` 表行 + 产物元数据）。 */
export interface BackupRecord {
  id: string;
  path: string;
  size_bytes: number;
  encrypted: boolean;
  /** `internal`（应用备份目录）/ `external`（用户选择的外部目录）。 */
  kind: string;
  created_at: number;
}

/** 容量状态（D13：2GB 警告 / 5GB 强烈提示）。 */
export interface BackupCapacity {
  db_bytes: number;
  wal_bytes: number;
  total_bytes: number;
  warn_bytes: number;
  critical_bytes: number;
  /** `ok` / `warn` / `critical`（`capacity-status` 的 `data-level`）。 */
  level: "ok" | "warn" | "critical";
}

export interface BackupListResponse {
  backups: BackupRecord[];
  capacity: BackupCapacity;
}

export interface BackupCreateResult {
  backup: BackupRecord;
  /** 保留策略实际清理的备份 id（D13：保留最近 10 份）。 */
  pruned: string[];
}

/** 恢复来源（ADR-004：内部备份 id 或外部 `.db` 路径）。 */
export type BackupRestoreSource =
  | { internal: { id: string } }
  | { external: { path: string } };

/** 恢复请求回执（`restart_required=true` = 已登记现场日志，待应用重启执行）。 */
export interface BackupRestoreResult {
  restoring: boolean;
  restart_required: boolean;
  source: "internal" | "external";
  candidate: {
    path: string;
    size_bytes: number;
    schema_version: number;
  };
  db_path: string;
}

export interface BackupIpc {
  list(): Promise<BackupListResponse>;
  create(label: string | null, targetDir: string | null): Promise<BackupCreateResult>;
  restore(source: BackupRestoreSource): Promise<BackupRestoreResult>;
  /** 系统目录选择器（复用 `startup_pick_target`）；`null` = 取消。 */
  pickTargetDir(): Promise<string | null>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const backupIpc: BackupIpc = {
  async list() {
    return invoke<BackupListResponse>("backup_list");
  },
  async create(label, targetDir) {
    return invoke<BackupCreateResult>("backup_create", {
      payload: {
        ...(label ? { label } : {}),
        ...(targetDir ? { target_dir: targetDir } : {}),
      },
    });
  },
  async restore(source) {
    return invoke<BackupRestoreResult>("backup_restore", {
      payload: { source },
    });
  },
  async pickTargetDir() {
    const result = await invoke<{ target_dir: string | null }>(
      "startup_pick_target",
    );
    return result.target_dir ?? null;
  },
};

/** 字节数展示（B/KiB/MiB/GiB，一位小数）。 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return index === 0
    ? `${Math.round(value)} ${units[index]}`
    : `${value.toFixed(1)} ${units[index]}`;
}
