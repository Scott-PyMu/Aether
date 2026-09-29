/**
 * 诊断导出 IPC 契约（M3-05；设计 D11/D13、ADR-003 决策 19，UI-UX S-07）。
 *
 * - `export_diagnostics` → `<target>/aether-diagnostics-<ts>.json`：整包经核心脱敏
 *   （密钥模式 0 命中，`scanned_clean=true` 为守门结果）；目标目录为外部路径
 *   （系统目录选择器选择；缺省复用 `startup_pick_target`，不新增命令）。
 *
 * 本文件是诊断命令面的类型化封装（可注入替身；生产 = Tauri invoke）。
 */
import { invoke } from "@tauri-apps/api/core";

import type { BackupCapacity } from "./backup";

/** 导出回执（与 Rust `diagnostics_control.rs` 的 JSON 形状一致）。 */
export interface DiagnosticsExportResult {
  path: string;
  file_name: string;
  bytes: number;
  generated_at: number;
  /** 实际包含的段（null 段被过滤；如降级态无库摘要）。 */
  sections: string[];
  /** 脱敏后再扫描结果（必须为 true）。 */
  scanned_clean: boolean;
  log_lines: number;
  task_dumps: number;
}

export interface DiagnosticsIpc {
  exportDiagnostics(targetDir: string): Promise<DiagnosticsExportResult>;
  /** 系统目录选择器（复用 `startup_pick_target`）；`null` = 取消。 */
  pickTargetDir(): Promise<string | null>;
  /** 容量状态（`backup_list` 投影；诊断页与右栏入口共用）。 */
  capacity(): Promise<BackupCapacity>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const diagnosticsIpc: DiagnosticsIpc = {
  async exportDiagnostics(targetDir) {
    return invoke<DiagnosticsExportResult>("export_diagnostics", {
      payload: { target_dir: targetDir },
    });
  },
  async pickTargetDir() {
    const result = await invoke<{ target_dir: string | null }>(
      "startup_pick_target",
    );
    return result.target_dir ?? null;
  },
  async capacity() {
    const result = await invoke<{ capacity: BackupCapacity }>("backup_list");
    return result.capacity;
  },
};
