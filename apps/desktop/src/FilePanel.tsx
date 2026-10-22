/**
 * 文件引用面板（S-11，M3-09；ADR-010 决策 1；UI-UX §2.6）。
 *
 * 只读引用面板（右栏常驻分区）：
 * - 子 tab「会话文件」= `kind=file` 引用；「项目文件」= 工作区根路径 + `kind=directory`
 *   引用（不递归）；**不列目录、不展开树、不预览内容、不读文件字节**；
 * - 添加文件 / 附加文件夹：`ref_pick`（系统选择器替身可注入）→ `artifact_add`；
 *   行内删除：`artifact_remove`（幂等）；
 * - 搜索仅过滤当前列表；空态文案「目录为空」（原型/ADR-010 决策 1）；
 * - `改动` tab 无入口（P0 隐藏）；
 * - 折叠/断点行为由右栏容器承担（≥1280 展开、<1280 抽屉；UI-UX §2.1）。
 *
 * 引用 ≠ 预授权：Agent 读取仍走 `fs.read` 权限门、写入仍走 `fs.write` 审批（D9 不变）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { artifactsIpc as productionArtifactsIpc, type ArtifactsIpc, type ArtifactEntry, type RefKind } from "./artifacts";
import { describeIpcError, ipcErrorCode } from "./startup";

type FileSubTab = "session" | "project";

export interface FilePanelProps {
  /** 当前激活会话（`null` = 未选择：显示引导空态，不发起 IPC）。 */
  sessionId: string | null;
  /** 当前会话绑定的工作区根（来自 `SessionSummary.workspace_root`；未绑定为 `null`）。 */
  workspaceRoot?: string | null;
  /** 引用 IPC（测试/E2E 注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: ArtifactsIpc;
}

/** 子 tab 与引用 kind 的映射（ADR-010 决策 1：文件 → 会话文件；目录 → 项目文件）。 */
const TAB_KIND: Record<FileSubTab, ArtifactEntry["kind"]> = {
  session: "file",
  project: "directory",
};

export function FilePanel({
  sessionId,
  workspaceRoot = null,
  ipc = productionArtifactsIpc,
}: FilePanelProps) {
  const [tab, setTab] = useState<FileSubTab>("session");
  const [artifacts, setArtifacts] = useState<ArtifactEntry[]>([]);
  const [search, setSearch] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);

  const reportError = useCallback((failure: unknown) => {
    setError(describeIpcError(failure));
    setErrorCode(ipcErrorCode(failure));
  }, []);
  const clearError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  const refresh = useCallback(async () => {
    if (!sessionId) {
      setArtifacts([]);
      return;
    }
    try {
      const list = await ipc.list(sessionId);
      setArtifacts(list);
      clearError();
    } catch (failure) {
      setArtifacts([]);
      reportError(failure);
    }
  }, [clearError, ipc, reportError, sessionId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const onAdd = useCallback(
    async (kind: RefKind) => {
      if (!sessionId || busy) {
        return;
      }
      setBusy(true);
      clearError();
      try {
        const picked = await ipc.pick(kind);
        if (picked !== null) {
          await ipc.add(sessionId, picked);
          await refresh();
        }
      } catch (failure) {
        reportError(failure);
      } finally {
        setBusy(false);
      }
    },
    [busy, clearError, ipc, refresh, reportError, sessionId],
  );

  const onRemove = useCallback(
    async (artifactId: string) => {
      if (!sessionId || busy) {
        return;
      }
      setBusy(true);
      clearError();
      try {
        await ipc.remove(sessionId, artifactId);
        await refresh();
      } catch (failure) {
        reportError(failure);
      } finally {
        setBusy(false);
      }
    },
    [busy, clearError, ipc, refresh, reportError, sessionId],
  );

  const current = useMemo(
    () => artifacts.filter((artifact) => artifact.kind === TAB_KIND[tab]),
    [artifacts, tab],
  );
  const keyword = search.trim().toLowerCase();
  const visible = useMemo(
    () =>
      keyword.length === 0
        ? current
        : current.filter((artifact) => artifact.path.toLowerCase().includes(keyword)),
    [current, keyword],
  );

  return (
    <section className="file-panel" data-testid="file-panel">
      <div className="r-search">
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <circle cx="11" cy="11" r="7" />
          <path d="M21 21l-4.35-4.35" />
        </svg>
        <input
          type="search"
          className="r-search-input"
          aria-label="搜索引用"
          placeholder="搜索文件..."
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
      </div>

      <div className="r-subtabs" role="tablist" aria-label="文件引用分类">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "session"}
          data-testid="file-panel-session-tab"
          data-active={String(tab === "session")}
          className={tab === "session" ? "r-subtab on" : "r-subtab"}
          onClick={() => setTab("session")}
        >
          会话文件
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "project"}
          data-testid="file-panel-project-tab"
          data-active={String(tab === "project")}
          className={tab === "project" ? "r-subtab on" : "r-subtab"}
          onClick={() => setTab("project")}
        >
          项目文件
        </button>
      </div>

      <p className="r-hint">
        支持拖拽文件或文件夹到输入框，实现引用；读取仍走权限门
      </p>
      {tab === "project" ? (
        <p className="file-panel-workspace">
          {workspaceRoot ? `工作区：${workspaceRoot}` : "未绑定工作区"}
        </p>
      ) : null}

      <div className="r-body">
        {!sessionId ? (
          <p className="r-empty" data-testid="file-panel-empty">
            请选择会话
          </p>
        ) : visible.length === 0 ? (
          <p className="r-empty" data-testid="file-panel-empty">
            目录为空
          </p>
        ) : (
          <ul className="file-panel-list">
            {visible.map((artifact) => (
              <li
                key={artifact.id}
                className="ref-item"
                data-testid="ref-item"
                data-ref-kind={tab}
                data-artifact-id={artifact.id}
                data-path={artifact.path}
              >
                <span className="ref-path" title={artifact.path}>
                  {artifact.path}
                </span>
                <span className="ref-meta">
                  {artifact.kind === "file" && artifact.size_bytes !== null
                    ? `${artifact.size_bytes} B`
                    : "目录"}
                </span>
                <button
                  type="button"
                  className="ref-remove"
                  data-testid="ref-remove"
                  disabled={busy}
                  aria-label={`移除引用 ${artifact.path}`}
                  onClick={() => void onRemove(artifact.id)}
                >
                  移除
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="r-btns">
        <button
          type="button"
          className="r-bigbtn"
          data-testid="ref-add-file"
          disabled={!sessionId || busy}
          onClick={() => void onAdd("file")}
        >
          添加文件
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M21 11l-9 9a6 6 0 01-8.5-8.5l9-9a4 4 0 015.5 5.5l-9 9a2 2 0 01-2.8-2.8l8-8" />
          </svg>
        </button>
        <button
          type="button"
          className="r-bigbtn"
          data-testid="ref-add-folder"
          disabled={!sessionId || busy}
          onClick={() => void onAdd("directory")}
        >
          附加文件夹
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z" />
          </svg>
        </button>
      </div>

      {error ? (
        <p
          className="file-panel-error"
          data-testid="ref-pick-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {errorCode === "artifact_path_rejected" ? `引用路径不可用：${error}` : error}
        </p>
      ) : null}
    </section>
  );
}
