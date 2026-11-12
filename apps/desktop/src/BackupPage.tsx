/**
 * 备份与恢复页（M3-04；M5-02 自动备份状态；M5-04 保留策略状态；M5-05 一键归档；
 * 设计 D13、ADR-020/ADR-021，UI-UX S-06/§7.3）。
 *
 * - 覆盖层管理视图（`overlay-backup` + `overlay-back`；由 App 视图状态机管理，不卸载
 *   工作台状态）；
 * - 手动备份：内部（默认 `backups/`）或外部目录（系统目录选择器 → `target_dir`）；
 *   空间不足由核心拒绝并在页面提示（ADR-003 决策 19）；
 * - 备份清单（`backup-item`：`data-backup-id`/`data-kind` manual|auto）+ 容量状态
 *   （`capacity-status`：`data-level` ok/warn/critical，D13 2GB/5GB）；
 * - 自动备份状态（`backup-auto-status`：开关/频率/保留/最近运行/下次排期；M5-02）；
 * - 保留策略状态（`retention-status`：开关/窗口/最近清理/归档失败；M5-04）；
 * - 一键归档（`archive`：`data-result`/`data-level`；缺省写入数据目录 `archives/`；
 *   `archive-external` 走系统目录选择器；M5-05）；
 * - 恢复：内部条目或外部 `.db` 路径 → 二次确认（`backup-restore-confirm`）→
 *   核心登记恢复请求（`restart_required`）→ 应用重启后由启动序列执行七步 3–6。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  archiveIpc as productionArchiveIpc,
  type ArchiveIpc,
} from "./archive";
import {
  backupIpc,
  formatBytes,
  type BackupAutoSegment,
  type BackupCapacity,
  type BackupIpc,
  type BackupRecord,
  type BackupRestoreSource,
  type BackupRetentionSegment,
} from "./backup";
import {
  importIpc as productionImportIpc,
  importIsCancellable,
  importIsTerminal,
  type ImportIpc,
  type ImportJobStatus,
  type ImportStatus,
} from "./import";
import {
  BACKUP_REMINDER_KEY,
  settingsIpc as productionSettingsIpc,
  type BackupReminder,
  type SettingsIpc,
} from "./settings";
import { describeIpcError, ipcErrorCode } from "./startup";
import { formatDateTime, t } from "./i18n";

export interface BackupPageProps {
  /** IPC 契约（缺省 = 生产 Tauri 实现；测试注入替身）。 */
  ipc?: BackupIpc;
  /** 设置 IPC（M3-05：备份提醒开关；缺省 = 生产实现）。 */
  settings?: SettingsIpc;
  /** 归档 IPC（M5-05：一键归档/会话导出；缺省 = 生产实现）。 */
  archive?: ArchiveIpc;
  /** 导入 IPC（M10-04/ADR-034；缺省 = 生产实现）。 */
  importer?: ImportIpc;
  /** 返回工作台（`overlay-back`）。 */
  onBack: () => void;
}

type PendingRestore =
  | { source: "internal"; id: string }
  | { source: "external"; path: string };

/** 时间展示（毫秒 epoch；`null` = 无）。 */
function formatTimestamp(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) {
    return "—";
  }
  return formatDateTime(ms);
}

/** 台账 kind 展示（ADR-020：manual=手动 / auto=自动）。 */
function kindLabel(kind: string): string {
  return kind === "auto" ? t("backup.auto") : t("backup.manual");
}

const IMPORT_STATUS_LABEL: Record<ImportStatus, string> = {
  staging: "backup.accepted_preparing_validation",
  validating: "backup.validating",
  applying: "backup.applying_staging",
  awaiting_restart: "backup.awaiting_restart_maintenance_window_read",
  completed: "app.common.completed",
  failed: "app.common.failed",
  cancelled: "app.common.cancelled",
};

export function BackupPage({
  ipc = backupIpc,
  settings = productionSettingsIpc,
  archive = productionArchiveIpc,
  importer = productionImportIpc,
  onBack,
}: BackupPageProps) {
  const [backups, setBackups] = useState<BackupRecord[]>([]);
  const [capacity, setCapacity] = useState<BackupCapacity | null>(null);
  const [reminder, setReminder] = useState<BackupReminder | null>(null);
  const [auto, setAuto] = useState<BackupAutoSegment | null>(null);
  const [retention, setRetention] = useState<BackupRetentionSegment | null>(null);
  const [label, setLabel] = useState("");
  const [externalPath, setExternalPath] = useState("");
  const [pending, setPending] = useState<PendingRestore | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [restoreResult, setRestoreResult] = useState<
    { result: "requested" | "failed"; source: "internal" | "external"; message: string } | null
  >(null);
  /** M5-05：一键归档（`archive`；`data-result` ok/failed）与外部目录归档状态。 */
  const [archiveBusy, setArchiveBusy] = useState(false);
  const [archiveResult, setArchiveResult] = useState<{
    result: "ok" | "failed";
    message: string;
  } | null>(null);
  /** M10-04：导入源路径/作业状态/提交失败。 */
  const [importSource, setImportSource] = useState("");
  const [importJob, setImportJob] = useState<ImportJobStatus | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [importError, setImportError] = useState<{
    message: string;
    code: string | null;
  } | null>(null);

  const refresh = useCallback(async () => {
    try {
      const response = await ipc.list();
      setBackups(response.backups);
      setCapacity(response.capacity);
      setReminder(response.reminder ?? null);
      setAuto(response.auto ?? null);
      setRetention(response.retention ?? null);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [ipc]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // M3-05/D13：7 天未备份提醒（可关闭）——「稍后」仅关闭提醒开关并刷新。
  const dismissReminder = useCallback(async () => {
    try {
      await settings.set(BACKUP_REMINDER_KEY, false);
      await refresh();
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [refresh, settings]);

  const resetMessages = useCallback(() => {
    setNotice(null);
    setError(null);
    setErrorCode(null);
  }, []);

  const create = useCallback(
    async (targetDir: string | null) => {
      setBusy(true);
      resetMessages();
      try {
        const created = await ipc.create(label.trim() ? label.trim() : null, targetDir);
        setNotice(
          t("backup.backup_complete", { p0: created.backup.path }) +
            (created.pruned.length > 0 ? t("backup.pruned_old_backups", { p0: created.pruned.length }) : ""),
        );
        await refresh();
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc, label, refresh, resetMessages],
  );

  const createExternal = useCallback(async () => {
    setBusy(true);
    resetMessages();
    try {
      const picked = await ipc.pickTargetDir();
      if (picked === null) {
        setNotice(t("backup.external_directory_selection_cancelled"));
        return;
      }
      await create(picked);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [create, ipc, resetMessages]);

  /** M5-05：一键归档（`archive_export`；缺省数据目录 `archives/`）。 */
  const archiveAll = useCallback(
    async (targetDir: string | null) => {
      setArchiveBusy(true);
      setArchiveResult(null);
      try {
        const response =
          targetDir === null
            ? await archive.exportAll()
            : await archive.exportAll(targetDir);
        setArchiveResult({
          result: "ok",
          message: t("backup.archive_complete_rows", { p0: response.export.path, p1: response.export.rows, p2: formatBytes(response.export.bytes) }),
        });
      } catch (failure) {
        setArchiveResult({ result: "failed", message: describeIpcError(failure) });
      } finally {
        setArchiveBusy(false);
      }
    },
    [archive],
  );

  /** M5-05：归档到外部目录（系统选择器 → `archive_export.target_dir`）。 */
  const archiveExternal = useCallback(async () => {
    setArchiveResult(null);
    try {
      const picked = await archive.pickTargetDir();
      if (picked === null) {
        setArchiveResult({ result: "failed", message: t("backup.external_directory_selection_cancelled") });
        return;
      }
      await archiveAll(picked);
    } catch (failure) {
      setArchiveResult({ result: "failed", message: describeIpcError(failure) });
    }
  }, [archive, archiveAll]);

  /** M10-04：作业非终态时 1s 轮询（`import_status`；awaiting_restart 停止轮询）。 */
  useEffect(() => {
    if (importJob === null || importIsTerminal(importJob.status)) {
      return;
    }
    const timer = window.setInterval(() => {
      void importer
        .status(importJob.job_id, true)
        .then((next) => setImportJob(next))
        .catch((failure: unknown) => {
          setImportError({
            message: describeIpcError(failure),
            code: ipcErrorCode(failure),
          });
        });
    }, 1000);
    return () => window.clearInterval(timer);
  }, [importer, importJob]);

  const pickImportSource = useCallback(async () => {
    setImportError(null);
    try {
      const picked = await importer.pickFile();
      if (picked !== null) {
        setImportSource(picked);
      }
    } catch (failure) {
      setImportError({
        message: describeIpcError(failure),
        code: ipcErrorCode(failure),
      });
    }
  }, [importer]);

  const startImport = useCallback(
    async (kind: "session" | "archive", dryRun: boolean) => {
      setImportBusy(true);
      setImportError(null);
      setImportJob(null);
      try {
        const source = importSource.trim();
        const started =
          kind === "archive"
            ? await importer.startArchive(source, "remap", dryRun)
            : await importer.startSession(source, "remap", dryRun);
        setImportJob(await importer.status(started.job_id, true));
      } catch (failure) {
        setImportError({
          message: describeIpcError(failure),
          code: ipcErrorCode(failure),
        });
      } finally {
        setImportBusy(false);
      }
    },
    [importSource, importer],
  );

  const cancelImport = useCallback(async () => {
    if (importJob === null) {
      return;
    }
    setImportBusy(true);
    try {
      await importer.cancel(importJob.job_id);
      setImportJob(await importer.status(importJob.job_id, true));
    } catch (failure) {
      setImportError({
        message: describeIpcError(failure),
        code: ipcErrorCode(failure),
      });
    } finally {
      setImportBusy(false);
    }
  }, [importJob, importer]);

  const restartForImport = useCallback(async () => {
    setImportBusy(true);
    try {
      await importer.restart();
    } catch (failure) {
      setImportError({
        message: describeIpcError(failure),
        code: ipcErrorCode(failure),
      });
      setImportBusy(false);
    }
  }, [importer]);

  const requestRestore = useCallback((source: PendingRestore) => {
    setPending(source);
    setRestoreResult(null);
    resetMessages();
  }, [resetMessages]);

  const confirmRestore = useCallback(async () => {
    if (pending === null) {
      return;
    }
    const source: BackupRestoreSource =
      pending.source === "internal"
        ? { internal: { id: pending.id } }
        : { external: { path: pending.path } };
    setBusy(true);
    resetMessages();
    try {
      const response = await ipc.restore(source);
      setRestoreResult({
        result: "requested",
        source: response.source,
        message: response.restart_required
          ? t("backup.restore_request_recorded_the_app")
          : t("backup.restore_request_recorded"),
      });
      setPending(null);
      await refresh();
    } catch (failure) {
      setRestoreResult({
        result: "failed",
        source: pending.source,
        message: describeIpcError(failure),
      });
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [ipc, pending, refresh, resetMessages]);

  const capacityLevel = capacity?.level ?? "ok";
  const capacityText = useMemo(() => {
    if (capacity === null) {
      return t("app.common.checking_capacity");
    }
    return (
      t("backup.database", { p0: formatBytes(capacity.total_bytes) }) +
      t("backup.db_wal", { p0: formatBytes(capacity.db_bytes), p1: formatBytes(capacity.wal_bytes) }) +
      t("backup.warning_threshold_critical", { p0: formatBytes(capacity.warn_bytes), p1: formatBytes(capacity.critical_bytes) })
    );
  }, [capacity]);

  return (
    <div className="overlay" data-testid="overlay-backup">
      <section className="backup-page" data-testid="backup-page" role="dialog" aria-modal="true">
        <header className="backup-header">
          <h2>{t("app.common.backup_and_restore")}</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            {t("app.action.back_to_workbench")}
          </button>
        </header>

        <p
          className="capacity-status"
          data-testid="capacity-status"
          data-level={capacityLevel}
          data-total-bytes={capacity?.total_bytes ?? 0}
          role={capacityLevel === "ok" ? undefined : "alert"}
        >
          {capacityLevel === "critical"
            ? t("backup.capacity_critical_5gb", { p0: capacityText })
            : capacityLevel === "warn"
              ? t("backup.capacity_warning_2gb", { p0: capacityText })
              : capacityText}
        </p>

        {reminder && reminder.enabled && reminder.due ? (
          <div
            className="backup-reminder"
            data-testid="backup-reminder"
            data-reason={reminder.reason ?? ""}
            role="alert"
          >
            <span>
              {reminder.reason === "never"
                ? t("backup.no_backup_has_been_created")
                : t("backup.more_than_7_days_since")}
            </span>
            <span className="backup-reminder-actions">
              <button type="button" disabled={busy} onClick={() => void create(null)}>
                {t("backup.back_up_now")}
              </button>
              <button
                type="button"
                data-testid="backup-reminder-dismiss"
                disabled={busy}
                onClick={() => void dismissReminder()}
              >
                {t("backup.dismiss_reminder")}
              </button>
            </span>
          </div>
        ) : null}

        {/* M5-05：容量一键归档（锚点 `archive`；`data-result` = ok/failed；`data-level`
            与 2GB/5GB 三档联动——警告/强提示档附上下文文案，归档目录缺省 `archives/`）。 */}
        <section className="archive-section">
          <div className="archive-actions">
            <button
              type="button"
              data-testid="archive"
              data-result={archiveResult?.result ?? ""}
              data-level={capacityLevel}
              disabled={archiveBusy}
              title={t("backup.archive_all_data_as_jsonl")}
              onClick={() => void archiveAll(null)}
            >
              {capacityLevel === "critical"
                ? t("backup.one_click_archive_capacity_critical")
                : capacityLevel === "warn"
                  ? t("backup.one_click_archive_capacity_warning")
                  : t("backup.one_click_archive")}
            </button>
            <button
              type="button"
              data-testid="archive-external"
              disabled={archiveBusy}
              onClick={() => void archiveExternal()}
            >
              {t("backup.archive_to_external_directory")}
            </button>
          </div>
          {archiveResult ? (
            <p
              className={
                archiveResult.result === "ok" ? "backup-notice" : "backup-error"
              }
              data-testid="archive-export-result"
              data-result={archiveResult.result}
              role={archiveResult.result === "failed" ? "alert" : "status"}
            >
              {archiveResult.message}
            </p>
          ) : null}
        </section>

        {/* M10-04/ADR-034：JSONL 导入入口/进度/警告/取消/失败恢复（S-06 复用）。 */}
        <section className="import-section" data-testid="import-section">
          <h3>{t("backup.import_adr_034")}</h3>
          <p className="import-hint">
            {t("backup.select_an_exported_jsonl_session")}
          </p>
          <div className="import-actions">
            <input
              type="text"
              data-testid="import-file"
              value={importSource}
              placeholder={t("backup.absolute_path_e_g_e")}
              onChange={(event) => setImportSource(event.target.value)}
            />
            <button
              type="button"
              data-testid="import-pick"
              disabled={importBusy}
              onClick={() => void pickImportSource()}
            >
              {t("backup.choose_file")}
            </button>
            <button
              type="button"
              data-testid="import"
              data-result={importJob?.status ?? ""}
              disabled={importBusy || importSource.trim().length === 0}
              onClick={() => void startImport("session", false)}
            >
              {t("backup.import_session")}
            </button>
            <button
              type="button"
              data-testid="import-archive"
              disabled={importBusy || importSource.trim().length === 0}
              onClick={() => void startImport("archive", false)}
            >
              {t("backup.import_archive")}
            </button>
            <button
              type="button"
              data-testid="import-dry-run"
              disabled={importBusy || importSource.trim().length === 0}
              onClick={() => void startImport("session", true)}
            >
              {t("backup.preview_only")}
            </button>
          </div>
          {importJob ? (
            <div
              className="import-status"
              data-testid="import-status"
              data-status={importJob.status}
              data-job-id={importJob.job_id}
              data-dry-run={String(importJob.dry_run)}
              role={importJob.status === "failed" ? "alert" : "status"}
            >
              <p>
                {t("backup.status")}{IMPORT_STATUS_LABEL[importJob.status]}{t("app.punct.open_paren")}{importJob.mode}{t("app.punct.semicolon")}
                {importJob.dry_run ? "dry-run" : t("backup.real_import")}{t("backup.progress")} {importJob.progress.rows} {t("backup.rows")}{" "}
                {formatBytes(importJob.progress.bytes)}
                {importJob.progress.total_bytes
                  ? ` / ${formatBytes(importJob.progress.total_bytes)}`
                  : ""}
              </p>
              {importJob.summary ? (
                <p data-testid="import-summary">
                  {t("app.common.sessions")} {importJob.summary.sessions} {t("backup.messages")} {importJob.summary.messages} {t("backup.events")}{" "}
                  {importJob.summary.events} {t("backup.runs")} {importJob.summary.runs} {t("backup.audit")}{" "}
                  {importJob.summary.audits} {t("backup.duplicates")} {importJob.summary.duplicates} {t("backup.skipped")}{" "}
                  {importJob.summary.skipped}
                </p>
              ) : null}
              {importJob.warnings && importJob.warnings.length > 0 ? (
                <ul data-testid="import-warnings">
                  {importJob.warnings.map((warning, index) => (
                    <li key={`${warning.code}-${index}`} data-code={warning.code}>
                      {warning.code}
                      {warning.path ? t("backup.text_3", { p0: warning.path }) : ""}{t("app.punct.colon")}{warning.detail}
                    </li>
                  ))}
                </ul>
              ) : null}
              {importJob.warnings_truncated ? (
                <p role="status">{t("backup.too_many_warnings_only_the")}</p>
              ) : null}
              {importJob.error ? (
                <p
                  className="backup-error"
                  data-testid="import-error"
                  data-error-code={importJob.error.code}
                  role="alert"
                >
                  {importJob.error.code}{t("app.punct.colon")}{importJob.error.message}
                </p>
              ) : null}
              <div className="import-status-actions">
                {importIsCancellable(importJob.status) ? (
                  <button
                    type="button"
                    data-testid="import-cancel"
                    disabled={importBusy}
                    onClick={() => void cancelImport()}
                  >
                    {t("backup.cancel_import")}
                  </button>
                ) : null}
                {importJob.status === "awaiting_restart" ? (
                  <button
                    type="button"
                    data-testid="import-restart"
                    disabled={importBusy}
                    onClick={() => void restartForImport()}
                  >
                    {t("backup.restart_now_to_complete_import")}
                  </button>
                ) : null}
              </div>
            </div>
          ) : null}
          {importError ? (
            <p
              className="backup-error"
              data-testid="import-submit-error"
              data-code={importError.code ?? ""}
              role="alert"
            >
              {importError.message}
            </p>
          ) : null}
        </section>

        {retention ? (
          <section
            className="backup-retention"
            data-testid="retention-status"
            data-enabled={String(retention.enabled)}
            data-level={retention.capacity.level}
            data-auto-armed={String(retention.auto_arm.armed)}
          >
            <h3>{t("app.common.event_retention_and_archiving_d13")}</h3>
            <p>
              {retention.enabled
                ? t("backup.enabled_retention_window_days_out", { p0: retention.days })
                : t("backup.disabled_all_events_are_retained")}
            </p>
            <p>
              {t("backup.last_cleanup")}
              {retention.last_run_at === null
                ? t("backup.none")
                : t("backup.rows_freed", { p0: retention.last_cleaned_rows, p1: formatBytes(retention.last_freed_bytes) })}
              {t("backup.archive_failures")} {retention.archive_failures_total} {t("backup.times")}
              {retention.growth.bytes_per_month !== null
                ? t("backup.monthly_growth_approx", { p0: formatBytes(Math.max(0, retention.growth.bytes_per_month)) })
                : ""}
            </p>
            {retention.auto_arm.armed ? (
              <p role="status">
                {t("backup.auto_enabled_by_threshold")}
                {retention.auto_arm.reason === "size"
                  ? t("backup.database_5gb_critical_tier")
                  : t("backup.event_growth_500mb_month")}
                {t("app.punct.close_paren")}
              </p>
            ) : null}
          </section>
        ) : null}

        {auto ? (
          <section
            className="backup-auto"
            data-testid="backup-auto-status"
            data-enabled={String(auto.enabled)}
            data-last-result={auto.last_result ?? ""}
            data-failures={auto.consecutive_failures}
            role={auto.consecutive_failures > 0 ? "alert" : undefined}
          >
            <h3>{t("app.common.automatic_backup_d13_adr_020")}</h3>
            <p>
              {auto.enabled ? t("app.common.enabled") : t("app.common.disabled")}{t("backup.frequency")}
              {auto.frequency === "weekly" ? t("backup.weekly") : t("backup.daily")}{t("backup.automatic_artifacts_retained_for")} {auto.retention} {t("backup.copies_manual_artifacts_are_not")}
            </p>
            <p>
              {t("backup.last_run")}{formatTimestamp(auto.last_run_at)}{t("app.punct.open_paren")}
              {auto.last_result === "success"
                ? t("app.common.success")
                : auto.last_result === "failed"
                  ? t("backup.failed_consecutive_times", { p0: auto.consecutive_failures })
                  : t("backup.none")}
              {t("backup.next_scheduled")}{formatTimestamp(auto.next_run_at)}
            </p>
          </section>
        ) : null}

        <section className="backup-create">
          <h3>{t("backup.manual_backup")}</h3>
          <label>
            {t("backup.label_optional")}
            <input
              type="text"
              data-testid="backup-create-label"
              value={label}
              maxLength={128}
              onChange={(event) => setLabel(event.target.value)}
            />
          </label>
          <div className="backup-create-actions">
            <button
              type="button"
              data-testid="backup-create"
              disabled={busy}
              onClick={() => void create(null)}
            >
              {t("backup.create_backup")}
            </button>
            <button
              type="button"
              data-testid="backup-create-external"
              disabled={busy}
              onClick={() => void createExternal()}
            >
              {t("backup.back_up_to_external_directory")}
            </button>
          </div>
          {notice ? (
            <p className="backup-notice" data-testid="backup-notice">
              {notice}
            </p>
          ) : null}
        </section>

        <section className="backup-list-section">
          <h3>
            {t("backup.backup_list_automatic_artifacts_retained")} {auto?.retention ?? 10} {t("backup.copies_manual_artifacts_are_not_2")}
          </h3>
          {backups.length === 0 ? (
            <p className="backup-empty" data-testid="backup-empty">
              {t("backup.no_backups_yet_automatic_backup")}
            </p>
          ) : (
            <ul className="backup-list" data-testid="backup-list">
              {backups.map((backup) => (
                <li
                  key={backup.id}
                  className="backup-item"
                  data-testid="backup-item"
                  data-backup-id={backup.id}
                  data-kind={backup.kind}
                >
                  <span className="backup-path" title={backup.path}>
                    {backup.path}
                  </span>
                  <span className="backup-meta">
                    {formatBytes(backup.size_bytes)} · {kindLabel(backup.kind)} ·{" "}
                    {formatDateTime(backup.created_at)}
                  </span>
                  <span className="backup-item-actions">
                    <button
                      type="button"
                      data-testid="backup-restore"
                      data-source="internal"
                      data-backup-id={backup.id}
                      disabled={busy}
                      onClick={() => requestRestore({ source: "internal", id: backup.id })}
                    >
                      {t("app.action.restore")}
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          )}

          <div className="backup-restore-external">
            <label>
              {t("backup.external_backup_file_db")}
              <input
                type="text"
                data-testid="backup-restore-external"
                value={externalPath}
                placeholder={t("backup.absolute_path_e_g_e_2")}
                onChange={(event) => setExternalPath(event.target.value)}
              />
            </label>
            <button
              type="button"
              data-testid="backup-restore-external-submit"
              disabled={busy || externalPath.trim().length === 0}
              onClick={() =>
                requestRestore({ source: "external", path: externalPath.trim() })
              }
            >
              {t("backup.restore_from_external_file")}
            </button>
          </div>
        </section>

        {pending ? (
          <section className="backup-restore-confirm-section">
            <p className="backup-restore-confirm-note">
              {t("backup.restore_will_replace_the_current")}
            </p>
            <button
              type="button"
              data-testid="backup-restore-confirm"
              data-source={pending.source}
              disabled={busy}
              onClick={() => void confirmRestore()}
            >
              {t("backup.confirm_restore")}
            </button>
            <button type="button" disabled={busy} onClick={() => setPending(null)}>
              {t("app.action.cancel")}
            </button>
          </section>
        ) : null}

        {restoreResult ? (
          <p
            className="backup-restore-result"
            data-testid="backup-restore-result"
            data-result={restoreResult.result}
            data-source={restoreResult.source}
            role={restoreResult.result === "failed" ? "alert" : undefined}
          >
            {restoreResult.message}
          </p>
        ) : null}

        {error ? (
          <p
            className="backup-error"
            data-testid="backup-error"
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
