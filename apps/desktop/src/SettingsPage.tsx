/**
 * 设置页（M3-05 + M3-08；M5-02 自动备份；设计 D10/D13/D14，UI-UX S-05/§7.3）。
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
 *
 * M5-02 范围（自动备份设置，D13/ADR-020）：
 * - `settings-backup-auto`（开关 / 频率 / 保留份数，经 `settings_set` 持久化；
 *   调度器每周期读取设置生效，无需重启）。
 *
 * M5-03 范围（加密备份开关，D13/ADR-020）：
 * - `settings-backup-encryption`（`backup.encryption.enabled`，缺省 `true`）；
 *   关闭后手动/自动备份产出 P0 `.db` 形态；开启且主密钥不可用时备份暂停并告警。
 *
 * M10-06 范围（外观，ADR-036）：
 * - S-15 `appearance`：`locale-select`（`ui.locale`）/ `theme-select`（`ui.theme`）；
 *   切换立即生效、不重启、不重载数据；持久化失败显示本地化错误。
 */
import { APP_VERSION } from "@aether/protocol";
import { useCallback, useEffect, useState } from "react";

import { useAppearance } from "./appearance";
import { localizeError, type LocaleSetting } from "./i18n";
import { PresetsPage } from "./PresetsPage";
import { presetsIpc as productionPresetsIpc, type PresetsIpc } from "./presets";
import { ProvidersPage } from "./ProvidersPage";
import { providersIpc as productionProvidersIpc, type ProvidersIpc } from "./providers";
import { rulesIpc as productionRulesIpc, type RulesIpc } from "./rules";
import { RulesPage } from "./RulesPage";
import {
  BACKUP_AUTO_ENABLED_KEY,
  BACKUP_AUTO_FREQUENCY_KEY,
  BACKUP_AUTO_RETENTION_KEY,
  BACKUP_ENCRYPTION_ENABLED_KEY,
  BACKUP_REMINDER_KEY,
  EVENTS_RETENTION_DAYS_KEY,
  EVENTS_RETENTION_ENABLED_KEY,
  settingsIpc,
  type SettingsIpc,
} from "./settings";
import { describeIpcError, ipcErrorCode } from "./startup";
import { workspaceIpc, type WorkspaceIpc } from "./workspace";
import { t } from "./i18n";

export interface SettingsSecurityLevel {
  level: "os" | "degraded";
  detail: string;
}

export interface SettingsPageProps {
  /** 数据目录（M9 起由 S-12 承接；主 UI 可选展示）。 */
  dataDir?: string;
  /** 安全级别（启动快照投影；缺省 = 未探测）。 */
  securityLevel?: SettingsSecurityLevel | null;
  /** IPC 契约（缺省 = typed client 生产实现；测试注入替身）。 */
  ipc?: SettingsIpc;
  /** 工作区绑定 IPC 契约（M3-08；缺省 = 生产实现；测试注入替身）。 */
  workspace?: WorkspaceIpc;
  /** 供应商配置 IPC 契约（M3-11；缺省 = 生产实现；测试注入替身）。 */
  providersIpc?: ProvidersIpc;
  /** 权限规则 IPC 契约（M6-01；缺省 = 生产实现；测试注入替身）。 */
  rulesIpc?: RulesIpc;
  /** Agent 预设 IPC 契约（M7-09；缺省 = 生产实现；测试注入替身）。 */
  presetsIpc?: PresetsIpc;
  /** 新建会话入口当前选中的预设（M7-09「使用中」标记数据源；`null` = 未选）。 */
  activePresetId?: string | null;
  /** 预设选择变更（与新建会话入口同步；缺省 = 只读展示）。 */
  onActivePresetChange?: (presetId: string | null) => void;
  /** 预设增改删成功后通知（工作台新建会话入口的预设清单需刷新；缺省 = 不通知）。 */
  onPresetsChanged?: () => void;
  onBack: () => void;
  onOpenBackup?: () => void;
  onOpenDiagnostics?: () => void;
  onOpenAbout?: () => void;
}

/** 设置页子导航（M3-11：基本设置 / 模型与供应商配置；M6-01：权限规则；M7-09：预设）。 */
export type SettingsTab = "general" | "providers" | "rules" | "presets";

export function SettingsPage({
  dataDir,
  securityLevel = null,
  ipc = settingsIpc,
  workspace = workspaceIpc,
  providersIpc = productionProvidersIpc,
  rulesIpc = productionRulesIpc,
  presetsIpc: presetsIpcProp = productionPresetsIpc,
  activePresetId = null,
  onActivePresetChange,
  onPresetsChanged,
  onBack,
  onOpenBackup,
  onOpenDiagnostics,
  onOpenAbout,
}: SettingsPageProps) {
  const [tab, setTab] = useState<SettingsTab>("general");
  const [reminderEnabled, setReminderEnabled] = useState<boolean | null>(null);
  // M5-02：自动备份设置（开关/频率/保留；缺省与核心登记一致）。
  const [autoEnabled, setAutoEnabled] = useState(true);
  const [autoFrequency, setAutoFrequency] = useState<"daily" | "weekly">("daily");
  const [autoRetention, setAutoRetention] = useState("10");
  // M5-03：加密备份开关（缺省开启；主密钥不可用时备份暂停并告警）。
  const [encryptionEnabled, setEncryptionEnabled] = useState(true);
  // M5-04：事件保留策略（开关 + 窗口 7–365 天；缺省关闭 / 30 天）。
  const [retentionEnabled, setRetentionEnabled] = useState(false);
  const [retentionDays, setRetentionDays] = useState("30");
  const [retentionLoaded, setRetentionLoaded] = useState(false);
  const [autoLoaded, setAutoLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // M3-08：工作区绑定（root_path 形式；输入可编辑，选择器可回填）。
  const [workspaceRoot, setWorkspaceRoot] = useState("");
  const [workspaceBusy, setWorkspaceBusy] = useState(false);
  // M10-06/ADR-036：外观（语言/主题）即时生效；持久化失败不恢复旧值但显示错误。
  const { localeSetting, themeSetting, setLocale, setTheme } = useAppearance();
  const [appearanceBusy, setAppearanceBusy] = useState(false);
  const [appearanceNotice, setAppearanceNotice] = useState<string | null>(null);
  const [appearanceError, setAppearanceError] = useState<string | null>(null);
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

  // M5-02/M5-03/M5-04：读取自动备份/加密/保留设置（缺省语义与核心一致：
  // 开启 / daily / 10 / 加密开启 / 保留关闭 / 30 天）。
  useEffect(() => {
    let active = true;
    Promise.all([
      ipc.get(BACKUP_AUTO_ENABLED_KEY),
      ipc.get(BACKUP_AUTO_FREQUENCY_KEY),
      ipc.get(BACKUP_AUTO_RETENTION_KEY),
      ipc.get(BACKUP_ENCRYPTION_ENABLED_KEY),
      ipc.get(EVENTS_RETENTION_ENABLED_KEY),
      ipc.get(EVENTS_RETENTION_DAYS_KEY),
    ])
      .then(([enabled, frequency, retention, encryption, retentionOn, retentionDaysValue]) => {
        if (!active) {
          return;
        }
        setAutoEnabled(enabled.value !== false);
        setAutoFrequency(frequency.value === "weekly" ? "weekly" : "daily");
        setAutoRetention(
          typeof retention.value === "number" ? String(retention.value) : "10",
        );
        setEncryptionEnabled(encryption.value !== false);
        setRetentionEnabled(retentionOn.value === true);
        setRetentionDays(
          typeof retentionDaysValue.value === "number"
            ? String(retentionDaysValue.value)
            : "30",
        );
        setAutoLoaded(true);
        setRetentionLoaded(true);
      })
      .catch((failure: unknown) => {
        if (active) {
          setError(describeIpcError(failure));
          setErrorCode(ipcErrorCode(failure));
          setAutoLoaded(true);
          setRetentionLoaded(true);
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
        setNotice(next ? t("settings.7_day_backup_reminder_enabled") : t("settings.7_day_backup_reminder_disabled"));
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  // M5-02：保存自动备份设置（校验后经 `settings_set` 持久化；调度器下周期生效）。
  const saveAutoBackup = useCallback(async () => {
    const retention = Number(autoRetention);
    if (!Number.isInteger(retention) || retention < 1 || retention > 100) {
      setError(t("settings.retention_count_must_be_an"));
      setErrorCode("out_of_range");
      return;
    }
    setBusy(true);
    setError(null);
    setErrorCode(null);
    setNotice(null);
    try {
      await ipc.set(BACKUP_AUTO_ENABLED_KEY, autoEnabled);
      await ipc.set(BACKUP_AUTO_FREQUENCY_KEY, autoFrequency);
      await ipc.set(BACKUP_AUTO_RETENTION_KEY, retention);
      setAutoRetention(String(retention));
      setNotice(t("settings.automatic_backup_settings_saved_effective"));
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [autoEnabled, autoFrequency, autoRetention, ipc]);

  // M5-03：加密备份开关（即时持久化；缺省开启）。
  const toggleEncryption = useCallback(
    async (next: boolean) => {
      setBusy(true);
      setError(null);
      setErrorCode(null);
      setNotice(null);
      try {
        const result = await ipc.set(BACKUP_ENCRYPTION_ENABLED_KEY, next);
        setEncryptionEnabled(result.value !== false);
        setNotice(
          next
            ? t("settings.encrypted_backup_enabled_xchacha20_poly1305")
            : t("settings.encrypted_backup_disabled_artifacts_are"),
        );
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  // M5-04：保存事件保留设置（开关 + 窗口 7–365 天；核心校验失败回结构化错误）。
  const saveRetention = useCallback(async () => {
    const days = Number(retentionDays);
    if (!Number.isInteger(days) || days < 7 || days > 365) {
      setError(t("settings.retention_window_must_be_an"));
      setErrorCode("out_of_range");
      return;
    }
    setBusy(true);
    setError(null);
    setErrorCode(null);
    setNotice(null);
    try {
      await ipc.set(EVENTS_RETENTION_ENABLED_KEY, retentionEnabled);
      await ipc.set(EVENTS_RETENTION_DAYS_KEY, days);
      setRetentionDays(String(days));
      setNotice(
        retentionEnabled
          ? t("settings.saved_event_retention_enabled_only")
          : t("settings.saved_event_retention_disabled_all"),
      );
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [ipc, retentionDays, retentionEnabled]);

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
      setError(t("settings.select_or_enter_a_workspace"));
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
        text: t("settings.workspace_bound_new_sessions_inject", { p0: result.root_path }),
      });
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
      setWorkspaceResult({ result: "failed", text: describeIpcError(failure) });
    } finally {
      setWorkspaceBusy(false);
    }
  }, [workspace, workspaceRoot]);

  const changeLocale = useCallback(
    async (value: LocaleSetting) => {
      setAppearanceBusy(true);
      setAppearanceNotice(null);
      setAppearanceError(null);
      try {
        await setLocale(value, ipc);
        setAppearanceNotice(t("settings.appearance.saved"));
      } catch (failure) {
        setAppearanceError(localizeError(failure).message);
      } finally {
        setAppearanceBusy(false);
      }
    },
    [ipc, setLocale],
  );

  const changeTheme = useCallback(
    async (value: "system" | "light" | "dark") => {
      setAppearanceBusy(true);
      setAppearanceNotice(null);
      setAppearanceError(null);
      try {
        await setTheme(value, ipc);
        setAppearanceNotice(t("settings.appearance.saved"));
      } catch (failure) {
        setAppearanceError(localizeError(failure).message);
      } finally {
        setAppearanceBusy(false);
      }
    },
    [ipc, setTheme],
  );

  const securityLevelValue = securityLevel?.level ?? "unknown";
  const securityLevelText =
    securityLevel === null
      ? t("settings.security_level_not_probed_os")
      : securityLevel.level === "os"
        ? t("settings.security_level_os_credential_store", { p0: securityLevel.detail })
        : t("settings.security_level_degraded", { p0: securityLevel.detail });

  return (
    <div className="overlay" data-testid="overlay-settings">
      <section
        className="settings-page modal"
        data-testid="settings-page"
        role="dialog"
        aria-modal="true"
      >
        <div className="modal-h">
          <h3>{t("settings.settings")}</h3>
          <button
            type="button"
            className="modal-close"
            data-testid="overlay-back"
            aria-label={t("settings.close_settings")}
            onClick={onBack}
          >
            ×
          </button>
        </div>

        <div className="settings-body">
          <nav className="settings-nav settings-subnav" aria-label={t("settings.settings_categories")}>
            <button
              type="button"
              data-testid="settings-tab-general"
              data-active={String(tab === "general")}
              className={tab === "general" ? "settings-tab on" : "settings-tab"}
              onClick={() => setTab("general")}
            >
              {t("settings.general")}
            </button>
            <button
              type="button"
              data-testid="settings-tab-providers"
              data-active={String(tab === "providers")}
              className={tab === "providers" ? "settings-tab on" : "settings-tab"}
              onClick={() => setTab("providers")}
            >
              {t("settings.model_provider_configuration")}
            </button>
            <button
              type="button"
              data-testid="settings-tab-rules"
              data-active={String(tab === "rules")}
              className={tab === "rules" ? "settings-tab on" : "settings-tab"}
              onClick={() => setTab("rules")}
            >
              {t("app.common.permission_rules")}
            </button>
            <button
              type="button"
              data-testid="settings-tab-presets"
              data-active={String(tab === "presets")}
              className={tab === "presets" ? "settings-tab on" : "settings-tab"}
              onClick={() => setTab("presets")}
            >
              {t("app.common.agent_presets")}
            </button>
            {onOpenAbout ? (
              <>
                <hr />
                <button type="button" onClick={onOpenAbout}>
                  {t("settings.about_aether")}
                </button>
              </>
            ) : null}
          </nav>

          <div className="settings-content">

        {tab === "providers" ? <ProvidersPage ipc={providersIpc} /> : null}

        {tab === "rules" ? <RulesPage ipc={rulesIpc} /> : null}

        {tab === "presets" ? (
          <PresetsPage
            ipc={presetsIpcProp}
            activePresetId={activePresetId}
            {...(onActivePresetChange ? { onActivePresetChange } : {})}
            {...(onPresetsChanged ? { onChanged: onPresetsChanged } : {})}
          />
        ) : null}

        {tab === "general" ? (
          <>
        <section className="settings-section settings-appearance" data-testid="appearance">
          <h3>{t("settings.appearance.title")}</h3>
          <p className="settings-note">{t("settings.appearance.description")}</p>
          <label className="settings-auto-row">
            {t("settings.appearance.locale")}
            <select
              data-testid="locale-select"
              data-locale={localeSetting}
              aria-label={t("a11y.locale_select")}
              value={localeSetting}
              disabled={appearanceBusy}
              onChange={(event) => void changeLocale(event.target.value as LocaleSetting)}
            >
              <option value="system">{t("settings.appearance.system")}</option>
              <option value="zh-CN">{t("settings.appearance.zh_cn")}</option>
              <option value="en-US">{t("settings.appearance.en_us")}</option>
            </select>
          </label>
          <label className="settings-auto-row">
            {t("settings.appearance.theme")}
            <select
              data-testid="theme-select"
              data-theme={themeSetting}
              aria-label={t("a11y.theme_select")}
              value={themeSetting}
              disabled={appearanceBusy}
              onChange={(event) =>
                void changeTheme(event.target.value as "system" | "light" | "dark")
              }
            >
              <option value="system">{t("settings.appearance.system")}</option>
              <option value="light">{t("settings.appearance.light")}</option>
              <option value="dark">{t("settings.appearance.dark")}</option>
            </select>
          </label>
          <p className="settings-note">{t("settings.appearance.system_hint")}</p>
          {appearanceNotice ? (
            <p className="settings-notice" data-testid="appearance-notice">
              {appearanceNotice}
            </p>
          ) : null}
          {appearanceError ? (
            <p className="settings-error" data-testid="appearance-error" role="alert">
              {appearanceError}
            </p>
          ) : null}
        </section>

        <section className="settings-section">
          <h3>{t("settings.data_security")}</h3>
          {dataDir ? (
            <p data-testid="settings-data-dir">{t("app.common.data_directory")}{dataDir}</p>
          ) : null}
          <p
            data-testid="settings-security-level"
            data-level={securityLevelValue}
            role={securityLevelValue === "degraded" ? "alert" : undefined}
          >
            {securityLevelText}
          </p>
          <p className="settings-note">
            {t("settings.security_boundary_c6_the_permission")}
          </p>
        </section>

        <section className="settings-section">
          <h3>{t("settings.reminders")}</h3>
          <label className="settings-toggle">
            <input
              type="checkbox"
              data-testid="settings-backup-reminder"
              checked={reminderEnabled === true}
              disabled={busy || reminderEnabled === null}
              onChange={(event) => void toggleReminder(event.target.checked)}
            />
            {t("settings.7_day_backup_reminder_d13")}
          </label>
          {notice ? (
            <p className="settings-notice" data-testid="settings-notice">
              {notice}
            </p>
          ) : null}
        </section>

        <section className="settings-section" data-testid="settings-backup-auto">
          <h3>{t("app.common.automatic_backup_d13_adr_020")}</h3>
          <label className="settings-toggle">
            <input
              type="checkbox"
              data-testid="settings-backup-auto-enabled"
              checked={autoEnabled}
              disabled={busy || !autoLoaded}
              onChange={(event) => setAutoEnabled(event.target.checked)}
            />
            {t("settings.enable_automatic_backup_on_by")}
          </label>
          <label className="settings-auto-row">
            {t("settings.frequency")}
            <select
              data-testid="settings-backup-auto-frequency"
              value={autoFrequency}
              disabled={busy || !autoLoaded}
              onChange={(event) =>
                setAutoFrequency(event.target.value === "weekly" ? "weekly" : "daily")
              }
            >
              <option value="daily">{t("settings.daily_daily")}</option>
              <option value="weekly">{t("settings.weekly_weekly")}</option>
            </select>
          </label>
          <label className="settings-auto-row">
            {t("settings.automatic_artifact_retention_count_1")}
            <input
              type="number"
              min={1}
              max={100}
              data-testid="settings-backup-auto-retention"
              value={autoRetention}
              disabled={busy || !autoLoaded}
              onChange={(event) => setAutoRetention(event.target.value)}
            />
          </label>
          <div className="settings-actions">
            <button
              type="button"
              data-testid="settings-backup-auto-save"
              disabled={busy || !autoLoaded}
              onClick={() => void saveAutoBackup()}
            >
              {t("settings.save_automatic_backup_settings")}
            </button>
          </div>
          <label className="settings-toggle">
            <input
              type="checkbox"
              data-testid="settings-backup-encryption"
              data-encryption={encryptionEnabled ? "on" : "off"}
              checked={encryptionEnabled}
              disabled={busy || !autoLoaded}
              onChange={(event) => void toggleEncryption(event.target.checked)}
            />
            {t("settings.encrypted_backup_xchacha20_poly1305_on")}
          </label>
          <p className="settings-note">
            {t("settings.automatic_artifacts_roll_by_retention")}
          </p>
        </section>

        <section className="settings-section" data-testid="settings-retention">
          <h3>{t("app.common.event_retention_and_archiving_d13")}</h3>
          <label className="settings-toggle">
            <input
              type="checkbox"
              data-testid="settings-retention-enabled"
              data-enabled={retentionEnabled ? "true" : "false"}
              checked={retentionEnabled}
              disabled={busy || !retentionLoaded}
              onChange={(event) => setRetentionEnabled(event.target.checked)}
            />
            {t("settings.enable_event_retention_off_by")}
          </label>
          <label className="settings-auto-row">
            {t("settings.retention_window_7_365_days")}
            <input
              type="number"
              min={7}
              max={365}
              data-testid="settings-retention-days"
              value={retentionDays}
              disabled={busy || !retentionLoaded}
              onChange={(event) => setRetentionDays(event.target.value)}
            />
          </label>
          <div className="settings-actions">
            <button
              type="button"
              data-testid="settings-retention-save"
              disabled={busy || !retentionLoaded}
              onClick={() => void saveRetention()}
            >
              {t("settings.save_retention_settings")}
            </button>
          </div>
          <p className="settings-note">
            {t("settings.prompted_at_capacity_2gb_auto")}
          </p>
        </section>

        <section className="settings-section">
          <h3>{t("settings.workspace_default_binding")}</h3>
          <p className="settings-note" data-testid="settings-workspace-entry">
            {t("settings.this_entry_only_sets_the")}
          </p>
          <label className="settings-workspace-row">
            {t("settings.workspace_root")}
            <input
              type="text"
              data-testid="workspace-root"
              value={workspaceRoot}
              placeholder={t("settings.select_or_enter_a_local")}
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
              {t("settings.choose_directory")}
            </button>
            <button
              type="button"
              data-testid="workspace-apply"
              disabled={workspaceBusy || workspaceRoot.trim().length === 0}
              onClick={() => void applyWorkspace()}
            >
              {t("settings.bind_workspace")}
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
            {t("settings.after_binding_new_sessions_inject")}
          </p>
        </section>

        <section className="settings-section">
          <h3>{t("settings.entry")}</h3>
          <div className="settings-actions">
            {onOpenBackup ? (
              <button type="button" data-testid="settings-open-backup" onClick={onOpenBackup}>
                {t("app.common.backup_and_restore")}
              </button>
            ) : null}
            {onOpenDiagnostics ? (
              <button
                type="button"
                data-testid="settings-open-diagnostics"
                onClick={onOpenDiagnostics}
              >
                {t("app.common.diagnostics_export")}
              </button>
            ) : null}
            {onOpenAbout ? (
              <button type="button" data-testid="settings-open-about" onClick={onOpenAbout}>
                {t("app.common.about")}
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
          </div>
        </div>

        <div className="settings-foot">
          <span>Aether v{APP_VERSION} · Apache-2.0</span>
          <span>{t("app.common.p0_runtimes_claude_code_codex")}</span>
        </div>
      </section>
    </div>
  );
}
