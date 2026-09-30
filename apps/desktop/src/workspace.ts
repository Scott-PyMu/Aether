/**
 * 工作区绑定 IPC 契约（M3-08；设计 D14/ADR-004 决策 3；UI-UX S-05/§7.3）。
 *
 * - 目录选择复用 `startup_pick_target` 系统选择器（M1-06 `DirectoryPicker` 抽象；
 *   不新增命令——口径同 M3-04 备份外部路径 / M3-05 诊断导出目标）；
 * - 绑定/切换经 `workspace_set`（`root_path` canonicalize + 同步盘拒绝在核心侧收口）；
 * - P0 语义：仅对新会话生效（记忆注入与权限基准目录）；已有会话不迁移（D14）。
 */
import { invoke } from "@tauri-apps/api/core";

/** `workspace_set` 回执（与 `workspaces` 表字段一致）。 */
export interface WorkspaceSetResult {
  workspace_id: string;
  name: string;
  root_path: string;
}

export interface WorkspaceIpc {
  /** 系统目录选择器（`null` = 取消）。 */
  pickDirectory(): Promise<string | null>;
  /** 绑定/切换工作区（`root_path` 形式；路径校验在核心命令层）。 */
  set(rootPath: string): Promise<WorkspaceSetResult>;
}

/** 生产实现（Tauri IPC）。 */
export const workspaceIpc: WorkspaceIpc = {
  async pickDirectory() {
    const result = await invoke<{ target_dir: string | null }>("startup_pick_target");
    return result.target_dir ?? null;
  },
  async set(rootPath) {
    return invoke<WorkspaceSetResult>("workspace_set", {
      payload: { root_path: rootPath },
    });
  },
};
