/**
 * 归档导出 IPC 契约（M5-05；设计 D13/D10、ADR-021、ADR-018）。
 *
 * 与 Rust 侧 `archive_control.rs` 的 JSON 形状一致（snake_case）：
 * - `session_export` → 单会话 JSONL（`sessions` 行 + `messages` 终稿 + 该会话
 *   `events` 按 `seq` 升序）+ manifest；缺省写入数据目录 `archives/`；
 * - `archive_export` → 全库归档（容量「一键归档」；锚点 `archive`）；
 *   外部目录复用 `startup_pick_target` 选择器（不新增命令）。
 *
 * 本文件是归档命令面的类型化封装（可注入替身；生产 = Tauri invoke）。
 */
import { invoke } from "@tauri-apps/api/core";

/** 归档计数（按行 kind）。 */
export interface ArchiveCounts {
  sessions: number;
  messages: number;
  events: number;
  runs: number;
  /** 审计行数（M6-03 `audit_export`；其余来源恒 0）。 */
  audits: number;
}

/** 归档产物报告（`{ export: {...} }` 载荷本体）。 */
export interface ArchiveReport {
  path: string;
  file_name: string;
  bytes: number;
  rows: number;
  sha256: string;
  /** `session-export` / `archive-export` / `auto-retention`。 */
  source: string;
  session_id: string | null;
  created_at: number;
  counts: ArchiveCounts;
}

export interface ArchiveExportResponse {
  export: ArchiveReport;
}

export interface ArchiveIpc {
  /** 单会话导出（缺省 `archives/`；`targetDir` = 外部目录）。 */
  exportSession(sessionId: string, targetDir?: string | null): Promise<ArchiveExportResponse>;
  /** 全库归档（缺省 `archives/`；`targetDir` = 外部目录）。 */
  exportAll(targetDir?: string | null): Promise<ArchiveExportResponse>;
  /** 系统目录选择器（复用 `startup_pick_target`）；`null` = 取消。 */
  pickTargetDir(): Promise<string | null>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const archiveIpc: ArchiveIpc = {
  async exportSession(sessionId, targetDir) {
    return invoke<ArchiveExportResponse>("session_export", {
      payload: {
        session_id: sessionId,
        ...(targetDir ? { target_dir: targetDir } : {}),
      },
    });
  },
  async exportAll(targetDir) {
    return invoke<ArchiveExportResponse>("archive_export", {
      payload: targetDir ? { target_dir: targetDir } : {},
    });
  },
  async pickTargetDir() {
    const result = await invoke<{ target_dir: string | null }>("startup_pick_target");
    return result.target_dir ?? null;
  },
};
