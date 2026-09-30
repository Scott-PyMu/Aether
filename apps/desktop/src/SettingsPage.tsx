/**
 * 设置页（M3-05 + M3-08；设计 D10/D13/D14，UI-UX S-05/§7.3）。
 *
 * M3-05 范围：
 * - 数据目录只读展示（`settings-data-dir`）；
 * - 安全级别只读展示（`settings-security-level`：`data-level` os/degraded；
 *   数据源为启动快照 `security_level`，即 A3 凭据库自检结果）；
 * - 备份提醒开关（`settings-backup-reminder`：`backup.reminder`，D13 7 天提醒可关闭；
 *   经 `settings_set` 持久化）；
 * - 跳转备份/诊断/关于。
 *
 * M3-08 范围（工作区绑定，D14/ADR-004 决策 3）：
 * - `workspace-pick`（复用 `startup_pick_target` 系统选择器）→ `workspace-root`
 *   输入展示/可编辑 → `workspace-apply`（`workspace_set`）→ `workspace-result`；
 * - P0 仅对新会话生效（记忆注入 + 权限基准目录）；已有会话不迁移。
 */
import { useCallback, useEffect, useState } from "react";

import { ProvidersPage } from "./ProvidersPage";
import { providersIpc as productionProvidersIpc, type ProvidersIpc } from "./providers";
import { BACKUP_REMINDER_KEY, settingsIpc, type SettingsIpc } from "./settings";
import { describeIpcError, ipcErrorCode } from "./startup";
import { workspaceIpc, type WorkspaceIpc } from "./workspace";

export interface SettingsSecurityLevel {
  level: "os" | "degraded";
  detail: string;
}

export interface SettingsPageProps {
  /** 数据目录（启动快照投影；只读展示）。 */
  dataDir: string;
  /** 安全级别（启动快照投影；缺省 = 未探测）。 */
  securityLevel?: SettingsSecurityLevel | null;
  /** IPC 契约（缺省 = 生产 Tauri 实现；测试注入替身）。 */
  ipc?: SettingsIpc;
  /** 工作区绑定 IPC 契约（M3-08；缺省 = 生产实现；测试注入替身）。 */
  workspace?: WorkspaceIpc;
  /** 供应商配置 IPC 契约（M3-11；缺省 = 生产实现；测试注入替身）。 */
  providersIpc?: ProvidersIpc;
  onBack: () => void;
  onOpenBackup?: () => void;
  onOpenDiagnostics?: () => void;
  onOpenAbout?: () => void;
}

/** 设置页子导航（M3-11：基本设置 / 模型与供应商配置；原型 settings-nav）。 */
export type SettingsTab = "general" | "providers";

export function SettingsPage({
  dataDir,
  securityLevel = null,
  ipc = settingsIpc,
  workspace = workspaceIpc,
  providersIpc = productionProvidersIpc,
  onBack,
  onOpenBackup,
  onOpenDiagnostics,
  onOpenAbout,
}: SettingsPageProps) {
  const [tab, setTab] = useState<SettingsTab>("general");
  const [reminderEnabled, setReminderEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // M3-08：工作区绑定（root_path 形式；输入可编辑，选择器可回填）。
  const [workspaceRoot, setWorkspaceRoot] = useState("");
  const [workspaceBusy, setWorkspaceBusy] = useState(false);
  const [workspaceResult, setWorkspaceResult] = useState<{
    result: "ok" | "failed";
    text: string;
  } | null>(null);

  useEffect(() => {
    let active = true;
    ipc
      .get(BACKUP_REMINDER_KEY)
      .then((result) => {
        if (active) {
          setReminderEnabled(result.value === true);
        }
      })
      .catch((failure: unknown) => {
        if (active) {
          setError(describeIpcError(failure));
          setErrorCode(ipcErrorCode(failure));
        }
      });
    return () => {
      active = false;
    };
  }, [ipc]);

  const toggleReminder = useCallback(
    async (next: boolean) => {
      setBusy(true);
      setError(null);
      setErrorCode(null);
      setNotice(null);
      try {
        const result = await ipc.set(BACKUP_REMINDER_KEY, next);
        setReminderEnabled(result.value === true);
        setNotice(next ? "已开启 7 天未备份提醒" : "已关闭 7 天未备份提醒");
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const pickWorkspace = useCallback(async () => {
    setError(null);
    setErrorCode(null);
    setWorkspaceResult(null);
    try {
      const picked = await workspace.pickDirectory();
      if (picked !== null) {
        setWorkspaceRoot(picked);
      }
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [workspace]);

  const applyWorkspace = useCallback(async () => {
    const root = workspaceRoot.trim();
    if (!root) {
      setError("请先选择或输入工作区根目录");
      setErrorCode("invalid_value");
      return;
    }
    setWorkspaceBusy(true);
    setError(null);
    setErrorCode(null);
    setWorkspaceResult(null);
    try {
      const result = await workspace.set(root);
      setWorkspaceRoot(result.root_path);
      setWorkspaceResult({
        result: "ok",
        text: `工作区已绑定：${result.root_path}（新会话按此目录注入记忆并作为权限基准；已有会话不迁移）`,
      });
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
      setWorkspaceResult({ result: "failed", text: describeIpcError(failure) });
    } finally {
      setWorkspaceBusy(false);
    }
  }, [workspace, workspaceRoot]);

  const securityLevelValue = securityLevel?.level ?? "unknown";
  const securityLevelText =
    securityLevel === null
      ? "安全级别：未探测（OS 凭据库自检结果随启动探针提供）"
      : securityLevel.level === "os"
        ? `安全级别：OS 凭据库（${securityLevel.detail}）`
        : `安全级别：降级（${securityLevel.detail}）`;

  return (
    <div className="overlay" data-testid="overlay-settings">
      <section
        className="settings-page"
        data-testid="settings-page"
        role="dialog"
        aria-modal="true"
      >
        <header className="settings-header">
          <h2>设置</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            返回工作台
          </button>
        </header>

        <nav className="settings-subnav" aria-label="设置分类">
          <button
            type="button"
            data-testid="settings-tab-general"
            data-active={String(tab === "general")}
            className={tab === "general" ? "settings-tab on" : "settings-tab"}
            onClick={() => setTab("general")}
          >
            基本设置
          </button>
          <button
            type="button"
            data-testid="settings-tab-providers"
            data-active={String(tab === "providers")}
            className={tab === "providers" ? "settings-tab on" : "settings-tab"}
            onClick={() => setTab("providers")}
          >
            模型与供应商配置
          </button>
        </nav>

        {tab === "providers" ? <ProvidersPage ipc={providersIpc} /> : null}

        {tab === "general" ? (
          <>
        <section className="settings-section">
          <h3>数据与安全</h3>
          <p data-testid="settings-data-dir">数据目录：{dataDir}</p>
          <p
            data-testid="settings-security-level"
            data-level={securityLevelValue}
            role={securityLevelValue === "degraded" ? "alert" : undefined}
          >
            {securityLevelText}
          </p>
          <p className="settings-note">
            安全边界（C6）：权限门仅约束经线协议上报的工具调用；适配器进程内行为不受此门约束。
          </p>
        </section>

        <section className="settings-section">
          <h3>提醒</h3>
          <label className="settings-toggle">
            <input
              type="checkbox"
              data-testid="settings-backup-reminder"
              checked={reminderEnabled === true}
              disabled={busy || reminderEnabled === null}
              onChange={(event) => void toggleReminder(event.target.checked)}
            />
            7 天未备份提醒（D13；全局开关）
          </label>
          {notice ? (
            <p className="settings-notice" data-testid="settings-notice">
              {notice}
            </p>
          ) : null}
        </section>

        <section className="settings-section">
          <h3>工作区</h3>
          <label className="settings-workspace-row">
            工作区根目录
            <input
              type="text"
              data-testid="workspace-root"
              value={workspaceRoot}
              placeholder="选择或输入本地目录（如 C:\\Projects\\demo）"
              disabled={workspaceBusy}
              onChange={(event) => setWorkspaceRoot(event.target.value)}
            />
          </label>
          <div className="settings-actions">
            <button
              type="button"
              data-testid="workspace-pick"
              disabled={workspaceBusy}
              onClick={() => void pickWorkspace()}
            >
              选择目录
            </button>
            <button
              type="button"
              data-testid="workspace-apply"
              disabled={workspaceBusy || workspaceRoot.trim().length === 0}
              onClick={() => void applyWorkspace()}
            >
              绑定工作区
            </button>
          </div>
          {workspaceResult ? (
            <p
              className={workspaceResult.result === "ok" ? "settings-notice" : "settings-error"}
              data-testid="workspace-result"
              data-result={workspaceResult.result}
              role={workspaceResult.result === "failed" ? "alert" : undefined}
            >
              {workspaceResult.text}
            </p>
          ) : null}
          <p data-testid="settings-workspace" className="settings-note">
            绑定后新会话注入工作区记忆（`AGENTS.md` &gt; `AETHER.md` &gt; `CLAUDE.md`，
            32KB 上限）并以其为权限基准目录；已有会话不迁移（P0，D14）。
          </p>
        </section>

        <section className="settings-section">
          <h3>入口</h3>
          <div className="settings-actions">
            {onOpenBackup ? (
              <button type="button" data-testid="settings-open-backup" onClick={onOpenBackup}>
                备份与恢复
              </button>
            ) : null}
            {onOpenDiagnostics ? (
              <button
                type="button"
                data-testid="settings-open-diagnostics"
                onClick={onOpenDiagnostics}
              >
                诊断导出
              </button>
            ) : null}
            {onOpenAbout ? (
              <button type="button" data-testid="settings-open-about" onClick={onOpenAbout}>
                关于
              </button>
            ) : null}
          </div>
        </section>
          </>
        ) : null}

        {error ? (
          <p
            className="settings-error"
            data-testid="settings-error"
            data-code={errorCode ?? ""}
            role="alert"
          >
            {error}
          </p>
        ) : null}
      </section>
    </div>
  );
}
