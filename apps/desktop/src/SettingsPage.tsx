/**
 * 设置页（M3-05；设计 D10/D13/D14，UI-UX S-05/§7.3）。
 *
 * M3-05 范围：
 * - 数据目录只读展示（`settings-data-dir`）；
 * - 安全级别只读展示（`settings-security-level`：`data-level` os/degraded；
 *   数据源为启动快照 `security_level`，即 A3 凭据库自检结果）；
 * - 备份提醒开关（`settings-backup-reminder`：`backup.reminder`，D13 7 天提醒可关闭；
 *   经 `settings_set` 持久化）；
 * - 跳转备份/诊断/关于。
 *
 * 工作区绑定（`settings-workspace`）归 M3-08；本页只放只读占位说明，不提供空按钮。
 */
import { useCallback, useEffect, useState } from "react";

import { BACKUP_REMINDER_KEY, settingsIpc, type SettingsIpc } from "./settings";
import { describeIpcError, ipcErrorCode } from "./startup";

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
  onBack: () => void;
  onOpenBackup?: () => void;
  onOpenDiagnostics?: () => void;
  onOpenAbout?: () => void;
}

export function SettingsPage({
  dataDir,
  securityLevel = null,
  ipc = settingsIpc,
  onBack,
  onOpenBackup,
  onOpenDiagnostics,
  onOpenAbout,
}: SettingsPageProps) {
  const [reminderEnabled, setReminderEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

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
          <p data-testid="settings-workspace" className="settings-placeholder">
            工作区绑定与权限基准目录由 M3-08 提供（P0 旧会话不迁移，D14）。
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
