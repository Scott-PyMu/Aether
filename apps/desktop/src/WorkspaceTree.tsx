/**
 * 左栏工作区树（M7-08/ADR-026 §2.1；UI-UX §7.3 锚点 `workspace-tree` /
 * `workspace-item` / `workspace-create`）。
 *
 * 职责（DoD5）：
 * - 工作区树：分组（每个工作区节点 + 未分配会话合成组「默认（数据目录）」）、
 *   计数（在途/总数）、折叠；
 * - `workspace_create` 入口（名称 + 目录选择器/手输）；改名、删除（二次确认 +
 *   `workspace_in_use` 提示）；会话改派（`session_workspace_assign`）；
 * - **入口去重**（ADR-026 §2.1-2）：本树不提供第二个全局绑定入口——设置页
 *   `workspace_set` 仍是「缺省工作区绑定（仅新会话生效）」，本树只管理与改派；
 * - 改派提示「权限基准不变，如需变更请新建会话」（R5/R15，不变量 11）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { SessionSummary } from "./session";
import {
  workspaceErrorOf,
  workspacesIpc as productionWorkspacesIpc,
  type SessionWorkspaceAssignResult,
  type WorkspaceIpcError,
  type WorkspaceSummary,
  type WorkspacesIpc,
  type WorkspacesListResult,
} from "./workspace";

export interface WorkspaceTreeProps {
  /** 工作区管理 IPC（测试注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: WorkspacesIpc;
  /** 会话列表（分组用；`session_list` 同源，含 `workspace_id`）。 */
  sessions: SessionSummary[];
  /** 当前聚焦会话（改派入口作用对象；`null` = 无会话 → 改派禁用）。 */
  activeSessionId: string | null;
  /** 点击会话叶子节点（打开/聚焦标签）。 */
  onOpenSession?: (sessionId: string) => void;
  /** 改派成功后的刷新钩子（会话列表 `workspace_id` 已变更）。 */
  onAssigned?: () => void | Promise<void>;
}

/** 未分配会话合成组的稳定键（`data-workspace-id=""`）。 */
export const UNASSIGNED_WORKSPACE_KEY = "";

/** 提示文案（ADR-026 §2.1-1/2：R15 基准固定）。 */
export const WORKSPACE_BASELINE_NOTICE = "权限基准不变，如需变更请新建会话";

const EMPTY_LIST: WorkspacesListResult = {
  workspaces: [],
  unassigned: { name: "默认（数据目录）", root_path: null, active_sessions: 0, total_sessions: 0 },
};

export function WorkspaceTree({
  ipc = productionWorkspacesIpc,
  sessions,
  activeSessionId,
  onOpenSession,
  onAssigned,
}: WorkspaceTreeProps) {
  const [list, setList] = useState<WorkspacesListResult>(EMPTY_LIST);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<WorkspaceIpcError | null>(null);
  /** 折叠状态（缺省全部展开；键为 `workspace_id` 或未分配组空串）。 */
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [createOpen, setCreateOpen] = useState(false);
  const [createName, setCreateName] = useState("");
  const [createRoot, setCreateRoot] = useState("");
  const [busy, setBusy] = useState(false);
  const [renameTarget, setRenameTarget] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [assignResult, setAssignResult] = useState<{
    result: "ok" | "failed";
    workspaceId?: string;
    baselineRoot?: string | null;
  } | null>(null);

  const refresh = useCallback(async () => {
    try {
      const next = await ipc.list();
      setList(next);
      setError(null);
    } catch (cause) {
      setError(workspaceErrorOf(cause));
    } finally {
      setLoading(false);
    }
  }, [ipc]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** `workspace_id → 会话`（未分配归入空串键；保持 `session_list` 顺序）。 */
  const sessionsByWorkspace = useMemo(() => {
    const grouped = new Map<string, SessionSummary[]>();
    for (const session of sessions) {
      const key = session.workspace_id ?? UNASSIGNED_WORKSPACE_KEY;
      const bucket = grouped.get(key);
      if (bucket) {
        bucket.push(session);
      } else {
        grouped.set(key, [session]);
      }
    }
    return grouped;
  }, [sessions]);

  const activeSession =
    activeSessionId === null
      ? undefined
      : sessions.find((session) => session.id === activeSessionId);

  const toggleCollapsed = (key: string) => {
    setCollapsed((previous) => {
      const next = new Set(previous);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  };

  const ensureIdle = () => {
    if (busy) {
      return false;
    }
    setBusy(true);
    setError(null);
    return true;
  };

  const handlePickRoot = async () => {
    try {
      const picked = await ipc.pickDirectory();
      if (picked !== null) {
        setCreateRoot(picked);
      }
    } catch (cause) {
      setError(workspaceErrorOf(cause));
      setBusy(false);
    }
  };

  const handleCreate = async () => {
    if (!ensureIdle()) {
      return;
    }
    try {
      await ipc.create({ name: createName.trim(), root_path: createRoot.trim() });
      setCreateOpen(false);
      setCreateName("");
      setCreateRoot("");
      await refresh();
    } catch (cause) {
      setError(workspaceErrorOf(cause));
    } finally {
      setBusy(false);
    }
  };

  const handleRename = async (workspaceId: string) => {
    if (!ensureIdle()) {
      return;
    }
    try {
      await ipc.update(workspaceId, renameDraft.trim());
      setRenameTarget(null);
      setRenameDraft("");
      await refresh();
    } catch (cause) {
      setError(workspaceErrorOf(cause));
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async (workspaceId: string) => {
    if (!ensureIdle()) {
      return;
    }
    try {
      await ipc.remove(workspaceId);
      setDeleteTarget(null);
      await refresh();
    } catch (cause) {
      setError(workspaceErrorOf(cause));
    } finally {
      setBusy(false);
    }
  };

  const handleAssign = async (workspaceId: string) => {
    if (!activeSession || !ensureIdle()) {
      return;
    }
    try {
      const result: SessionWorkspaceAssignResult = await ipc.assignSession(
        activeSession.id,
        workspaceId,
      );
      setAssignResult({
        result: "ok",
        workspaceId,
        baselineRoot: result.baseline_root,
      });
      await refresh();
      await onAssigned?.();
    } catch (cause) {
      setAssignResult({ result: "failed", workspaceId });
      setError(workspaceErrorOf(cause));
    } finally {
      setBusy(false);
    }
  };

  const renderWorkspaceNode = (
    key: string,
    name: string,
    rootPath: string | null,
    active: number,
    total: number,
    options: { workspaceId: string | null; manageable: boolean },
  ) => {
    const nodeSessions = sessionsByWorkspace.get(key) ?? [];
    const isCollapsed = collapsed.has(key);
    const assignable =
      options.manageable &&
      activeSession !== undefined &&
      activeSession.workspace_id !== options.workspaceId &&
      !busy;
    return (
      <li
        key={key === "" ? "__unassigned__" : key}
        className="workspace-item"
        data-testid="workspace-item"
        data-workspace-id={key}
        data-manageable={String(options.manageable)}
      >
        <div className="workspace-item-row">
          <button
            type="button"
            className="workspace-item-toggle"
            data-testid="workspace-item-toggle"
            aria-expanded={!isCollapsed}
            onClick={() => toggleCollapsed(key)}
          >
            {isCollapsed ? "▸" : "▾"}
          </button>
          <button
            type="button"
            className="workspace-item-name"
            data-testid="workspace-item-name"
            data-workspace-id={key}
            title={rootPath ?? undefined}
            onClick={() => toggleCollapsed(key)}
          >
            {name}
          </button>
          <span
            className="workspace-item-count"
            data-testid="workspace-item-count"
            data-active={String(active)}
            data-total={String(total)}
            title="在途会话 / 全部会话"
          >
            {active}/{total}
          </span>
        </div>
        {options.manageable ? (
          <div className="workspace-item-actions">
            <button
              type="button"
              data-testid="workspace-assign"
              data-workspace-id={key}
              data-session-id={activeSessionId ?? ""}
              disabled={!assignable}
              title={
                activeSession === undefined
                  ? "先打开一个会话，再改派工作区"
                  : activeSession.workspace_id === options.workspaceId
                    ? "会话已在该工作区"
                    : WORKSPACE_BASELINE_NOTICE
              }
              onClick={() => void handleAssign(key)}
            >
              改派当前会话
            </button>
            <button
              type="button"
              data-testid="workspace-rename"
              data-workspace-id={key}
              onClick={() => {
                setRenameTarget(key);
                setRenameDraft(name);
              }}
            >
              改名
            </button>
            <button
              type="button"
              data-testid="workspace-delete"
              data-workspace-id={key}
              onClick={() => setDeleteTarget(key)}
            >
              删除
            </button>
          </div>
        ) : null}
        {renameTarget === key ? (
          <div className="workspace-rename-form">
            <input
              data-testid="workspace-rename-input"
              data-workspace-id={key}
              value={renameDraft}
              maxLength={64}
              onChange={(event) => setRenameDraft(event.target.value)}
            />
            <button
              type="button"
              data-testid="workspace-rename-save"
              disabled={busy || renameDraft.trim() === ""}
              onClick={() => void handleRename(key)}
            >
              保存
            </button>
            <button
              type="button"
              data-testid="workspace-rename-cancel"
              onClick={() => setRenameTarget(null)}
            >
              取消
            </button>
          </div>
        ) : null}
        {deleteTarget === key ? (
          <div className="workspace-delete-confirm" data-testid="workspace-delete-confirm-box">
            <p>删除工作区将解除绑定（不删除磁盘文件），不可恢复。</p>
            <button
              type="button"
              data-testid="workspace-delete-confirm"
              disabled={busy}
              onClick={() => void handleDelete(key)}
            >
              确认删除
            </button>
            <button
              type="button"
              data-testid="workspace-delete-cancel"
              onClick={() => setDeleteTarget(null)}
            >
              取消
            </button>
          </div>
        ) : null}
        {isCollapsed ? null : nodeSessions.length === 0 ? (
          <p className="workspace-session-empty" data-testid="workspace-session-empty">
            （无会话）
          </p>
        ) : (
          <ul className="workspace-session-list">
            {nodeSessions.map((session) => (
              <li key={session.id}>
                <button
                  type="button"
                  className={
                    session.id === activeSessionId
                      ? "workspace-session active"
                      : "workspace-session"
                  }
                  data-testid="workspace-session"
                  data-session-id={session.id}
                  data-active={String(session.id === activeSessionId)}
                  data-status={session.status}
                  onClick={() => onOpenSession?.(session.id)}
                >
                  {session.title}
                </button>
              </li>
            ))}
          </ul>
        )}
      </li>
    );
  };

  return (
    <section className="workspace-tree" data-testid="workspace-tree" aria-label="工作区">
      <div className="workspace-tree-header">
        <h2 className="workbench-title">工作区</h2>
        <button
          type="button"
          data-testid="workspace-create"
          aria-expanded={createOpen}
          onClick={() => {
            setCreateOpen((open) => !open);
            setError(null);
          }}
        >
          + 新建
        </button>
      </div>
      <p className="workspace-tree-notice" data-testid="workspace-assign-notice">
        {WORKSPACE_BASELINE_NOTICE}
      </p>
      {error ? (
        <p
          className="workspace-error"
          data-testid="workspace-error"
          data-code={error.code}
          role="alert"
        >
          {error.message}
        </p>
      ) : null}
      {assignResult ? (
        <p
          className="workspace-assign-result"
          data-testid="workspace-assign-result"
          data-result={assignResult.result}
        >
          {assignResult.result === "ok"
            ? `已改派（工作区 ${assignResult.workspaceId}）；${WORKSPACE_BASELINE_NOTICE}${
                assignResult.baselineRoot ? `（当前基准：${assignResult.baselineRoot}）` : ""
              }`
            : "改派失败（见错误提示）"}
        </p>
      ) : null}
      {createOpen ? (
        <div className="workspace-create-form" data-testid="workspace-create-form">
          <input
            data-testid="workspace-name-input"
            placeholder="工作区名称（≤64 字符）"
            maxLength={64}
            value={createName}
            onChange={(event) => setCreateName(event.target.value)}
          />
          <div className="workspace-root-row">
            <input
              data-testid="workspace-root-input"
              placeholder="工作区根目录（绝对路径）"
              value={createRoot}
              onChange={(event) => setCreateRoot(event.target.value)}
            />
            <button
              type="button"
              data-testid="workspace-root-pick"
              onClick={() => void handlePickRoot()}
            >
              选择…
            </button>
          </div>
          <div className="workspace-create-actions">
            <button
              type="button"
              data-testid="workspace-create-submit"
              disabled={busy || createName.trim() === "" || createRoot.trim() === ""}
              onClick={() => void handleCreate()}
            >
              创建工作区
            </button>
            <button
              type="button"
              data-testid="workspace-create-cancel"
              onClick={() => setCreateOpen(false)}
            >
              取消
            </button>
          </div>
          <p className="workspace-create-hint">
            设置页的「工作区绑定」= 缺省工作区（仅新会话生效）；此处用于管理与改派既有会话。
          </p>
        </div>
      ) : null}
      {loading ? (
        <p className="workspace-tree-loading" data-testid="workspace-tree-loading">
          加载工作区…
        </p>
      ) : (
        <ul className="workspace-tree-list">
          {list.workspaces.map((workspace: WorkspaceSummary) =>
            renderWorkspaceNode(
              workspace.workspace_id,
              workspace.name,
              workspace.root_path,
              workspace.active_sessions,
              workspace.total_sessions,
              { workspaceId: workspace.workspace_id, manageable: true },
            ),
          )}
          {renderWorkspaceNode(
            UNASSIGNED_WORKSPACE_KEY,
            list.unassigned.name,
            list.unassigned.root_path,
            list.unassigned.active_sessions,
            list.unassigned.total_sessions,
            { workspaceId: null, manageable: false },
          )}
        </ul>
      )}
      {!loading && list.workspaces.length === 0 && list.unassigned.total_sessions === 0 ? (
        <p className="workspace-tree-empty" data-testid="workspace-tree-empty">
          暂无工作区；点击「+ 新建」添加。
        </p>
      ) : null}
    </section>
  );
}
