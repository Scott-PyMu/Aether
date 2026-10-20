/**
 * 工作区 IPC 契约（M3-08 绑定 + M7-08 管理与多工作区；设计 D14/ADR-004/ADR-026；
 * UI-UX S-05/§7.3）。
 *
 * - 目录选择复用 `startup_pick_target` 系统选择器（M1-06 `DirectoryPicker` 抽象；
 *   不新增命令——口径同 M3-04 备份外部路径 / M3-05 诊断导出目标）；
 * - 绑定/切换经 `workspace_set`（`root_path` canonicalize + 同步盘拒绝在核心侧收口）；
 * - M7-08/R15 语义：`workspace_set` 设置**缺省**工作区（记忆注入与权限基准目录），
 *   **仅对新会话生效**；既有会话保持创建时权限基准（左栏树改派不重算）。
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

// ===== M7-08/ADR-026 §2.1：工作区管理与多工作区（左栏树）=====

/** 工作区条目（`workspaces_list.workspaces[]`；ADO-026 §2.1-1）。 */
export interface WorkspaceSummary {
  workspace_id: string;
  name: string;
  root_path: string;
  /** ADR-026 §2.1-4：P1 不启用（恒空数组）。 */
  memory_files: string[];
  /** 在途会话数（非终态状态或存在 queued/running run）。 */
  active_sessions: number;
  /** 全部会话数（含终态）。 */
  total_sessions: number;
  created_at: number;
  updated_at: number;
}

/** 未分配会话合成组（`workspace_id IS NULL`；不建 `workspaces` 行）。 */
export interface WorkspaceUnassigned {
  name: string;
  root_path: string | null;
  active_sessions: number;
  total_sessions: number;
}

/** `workspaces_list` 回执。 */
export interface WorkspacesListResult {
  workspaces: WorkspaceSummary[];
  unassigned: WorkspaceUnassigned;
}

/** `session_workspace_assign` 回执（ADR-026 §2.1-1/2；R15 基准不变）。 */
export interface SessionWorkspaceAssignResult {
  session_id: string;
  workspace_id: string;
  previous_workspace_id: string | null;
  changed: boolean;
  /** R15/不变量 11：改派不重算权限基准。 */
  baseline_unchanged: boolean;
  /** 创建时固定的权限基准根（`null` = 数据目录基准）。 */
  baseline_root: string | null;
  /** 记忆注入源是否已切换（对新 run 生效；既有适配器会话上下文不追溯）。 */
  memory_injection_switched: boolean;
  /** UI 提示文案（ADR-026 §2.1-1：「权限基准不变，如需变更请新建会话」）。 */
  notice: string;
}

/** `workspace_delete` 回执（只删行、不删磁盘文件）。 */
export interface WorkspaceDeleteResult {
  workspace_id: string;
  deleted: boolean;
  disk_untouched: boolean;
}

/**
 * 工作区管理 IPC（M7-08 五命令；ADR-026 §2.1）。
 *
 * 与 [`WorkspaceIpc`]（S-05 设置页的缺省绑定）分属两个入口语义：
 * - 本接口 = 左栏树「工作区管理 / 会话改派」（不提供第二个全局绑定入口）；
 * - `WorkspaceIpc.set` = 设置页缺省工作区绑定（仅新会话生效）。
 */
export interface WorkspacesIpc {
  list(): Promise<WorkspacesListResult>;
  create(input: { name: string; root_path: string }): Promise<WorkspaceSummary>;
  update(workspaceId: string, name: string): Promise<WorkspaceSummary>;
  remove(workspaceId: string): Promise<WorkspaceDeleteResult>;
  assignSession(sessionId: string, workspaceId: string): Promise<SessionWorkspaceAssignResult>;
  /** 系统目录选择器（`null` = 取消；复用 `startup_pick_target`）。 */
  pickDirectory(): Promise<string | null>;
}

/** 生产实现（Tauri IPC；五命令 + 目录选择器）。 */
export const workspacesIpc: WorkspacesIpc = {
  async list() {
    return invoke<WorkspacesListResult>("workspaces_list", { payload: {} });
  },
  async create(input) {
    return invoke<WorkspaceSummary>("workspace_create", { payload: input });
  },
  async update(workspaceId, name) {
    return invoke<WorkspaceSummary>("workspace_update", {
      payload: { workspace_id: workspaceId, name },
    });
  },
  async remove(workspaceId) {
    return invoke<WorkspaceDeleteResult>("workspace_delete", {
      payload: { workspace_id: workspaceId },
    });
  },
  async assignSession(sessionId, workspaceId) {
    return invoke<SessionWorkspaceAssignResult>("session_workspace_assign", {
      payload: { session_id: sessionId, workspace_id: workspaceId },
    });
  },
  async pickDirectory() {
    const result = await invoke<{ target_dir: string | null }>("startup_pick_target");
    return result.target_dir ?? null;
  },
};

/** 结构化 IPC 错误（前端按 `code` 分支；`message` 仅展示）。 */
export interface WorkspaceIpcError {
  code: string;
  message: string;
  field?: string;
}

/** 错误对象归一（非结构化错误 → `internal`）。 */
export function workspaceErrorOf(error: unknown): WorkspaceIpcError {
  if (typeof error === "object" && error !== null) {
    const candidate = error as Partial<WorkspaceIpcError>;
    if (typeof candidate.code === "string") {
      return {
        code: candidate.code,
        message: typeof candidate.message === "string" ? candidate.message : candidate.code,
        field: typeof candidate.field === "string" ? candidate.field : undefined,
      };
    }
  }
  return { code: "internal", message: String(error) };
}
