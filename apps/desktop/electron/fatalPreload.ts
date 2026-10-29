/**
 * S-12 壳级致命页 preload（M9-07 DoD-4/DoD-6；ADR-029 §3.5-8）。
 *
 * 这是**独立最小 renderer** 的专用 preload，与主 UI 的 `window.aether` 完全分离：
 * 不暴露 rpc/命令面/Node/fs，只提供 S-12 所需四个动作：
 * 读取状态、重试、退出、离线导出诊断。
 */
import { contextBridge, ipcRenderer } from "electron";

import {
  FATAL_EXIT_CHANNEL,
  FATAL_EXPORT_CHANNEL,
  FATAL_RETRY_CHANNEL,
  FATAL_STATE_CHANNEL,
} from "./shellPageChannels";

export interface ShellFatalViewState {
  reason:
    | "daemon_start_failed"
    | "handshake_failed"
    | "a4_blocked"
    | "db_open_failed"
    | "schema_checksum_mismatch"
    | "data_dir_locked";
  message: string;
  logPath: string;
  dataDir: string;
  attempts: number;
  /** 手动移动数据目录说明（纯文本，UI-UX S-12）。 */
  manualMove: string;
}

const api = {
  getState(): Promise<ShellFatalViewState> {
    return ipcRenderer.invoke(FATAL_STATE_CHANNEL) as Promise<ShellFatalViewState>;
  },
  retry(): Promise<{ ok: boolean }> {
    return ipcRenderer.invoke(FATAL_RETRY_CHANNEL) as Promise<{ ok: boolean }>;
  },
  exit(): Promise<{ ok: boolean }> {
    return ipcRenderer.invoke(FATAL_EXIT_CHANNEL) as Promise<{ ok: boolean }>;
  },
  exportDiagnostics(targetPath?: string): Promise<{ ok: boolean; path?: string; error?: string }> {
    return ipcRenderer.invoke(FATAL_EXPORT_CHANNEL, targetPath) as Promise<{
      ok: boolean;
      path?: string;
      error?: string;
    }>;
  },
};

contextBridge.exposeInMainWorld("aetherFatal", Object.freeze(api));
