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
  BACKUP_REMINDER_KEY,
  settingsIpc as productionSettingsIpc,
  type BackupReminder,
  type SettingsIpc,
} from "./settings";
import { describeIpcError, ipcErrorCode } from "./startup";

export interface BackupPageProps {
  /** IPC 契约（缺省 = 生产 Tauri 实现；测试注入替身）。 */
  ipc?: BackupIpc;
  /** 设置 IPC（M3-05：备份提醒开关；缺省 = 生产实现）。 */
  settings?: SettingsIpc;
  /** 归档 IPC（M5-05：一键归档/会话导出；缺省 = 生产实现）。 */
  archive?: ArchiveIpc;
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
  return new Date(ms).toLocaleString();
}

/** 台账 kind 展示（ADR-020：manual=手动 / auto=自动）。 */
function kindLabel(kind: string): string {
  return kind === "auto" ? "自动" : "手动";
}

export function BackupPage({
  ipc = backupIpc,
  settings = productionSettingsIpc,
  archive = productionArchiveIpc,
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
          `备份完成：${created.backup.path}` +
            (created.pruned.length > 0 ? `（清理旧备份 ${created.pruned.length} 份）` : ""),
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
        setNotice("已取消外部目录选择");
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
          message: `归档完成：${response.export.path}（${response.export.rows} 行，${formatBytes(response.export.bytes)}）`,
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
        setArchiveResult({ result: "failed", message: "已取消外部目录选择" });
        return;
      }
      await archiveAll(picked);
    } catch (failure) {
      setArchiveResult({ result: "failed", message: describeIpcError(failure) });
    }
  }, [archive, archiveAll]);

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
          ? "恢复请求已登记：应用即将重启，重启后由启动序列完成恢复（修复外部条件后仍失败会保留旧数据）"
          : "恢复请求已登记",
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
      return "容量查询中…";
    }
    return (
      `数据库 ${formatBytes(capacity.total_bytes)}` +
      `（db ${formatBytes(capacity.db_bytes)} + wal ${formatBytes(capacity.wal_bytes)}）` +
      `；警告阈值 ${formatBytes(capacity.warn_bytes)} / 强提示 ${formatBytes(capacity.critical_bytes)}`
    );
  }, [capacity]);

  return (
    <div className="overlay" data-testid="overlay-backup">
      <section className="backup-page" data-testid="backup-page" role="dialog" aria-modal="true">
        <header className="backup-header">
          <h2>备份与恢复</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            返回工作台
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
            ? `容量紧张（≥5GB）：${capacityText}`
            : capacityLevel === "warn"
              ? `容量警告（≥2GB）：${capacityText}`
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
                ? "尚未创建过备份（D13：建议定期手动备份）。"
                : "距上次备份已超过 7 天（D13：建议尽快备份）。"}
            </span>
            <span className="backup-reminder-actions">
              <button type="button" disabled={busy} onClick={() => void create(null)}>
                立即备份
              </button>
              <button
                type="button"
                data-testid="backup-reminder-dismiss"
                disabled={busy}
                onClick={() => void dismissReminder()}
              >
                关闭提醒
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
              title="归档全部数据为 JSONL（含 manifest；缺省写入数据目录 archives/）"
              onClick={() => void archiveAll(null)}
            >
              {capacityLevel === "critical"
                ? "一键归档（容量紧张，强烈建议）"
                : capacityLevel === "warn"
                  ? "一键归档（容量警告）"
                  : "一键归档"}
            </button>
            <button
              type="button"
              data-testid="archive-external"
              disabled={archiveBusy}
              onClick={() => void archiveExternal()}
            >
              归档到外部目录…
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

        {retention ? (
          <section
            className="backup-retention"
            data-testid="retention-status"
            data-enabled={String(retention.enabled)}
            data-level={retention.capacity.level}
            data-auto-armed={String(retention.auto_arm.armed)}
          >
            <h3>事件保留与归档（D13 / ADR-021）</h3>
            <p>
              {retention.enabled
                ? `已启用：保留窗口 ${retention.days} 天（超窗 message.delta/usage/log 先归档后清理）`
                : "未启用：事件全量保留；容量 ≥2GB 时建议在设置中开启"}
            </p>
            <p>
              最近清理：
              {retention.last_run_at === null
                ? "暂无"
                : `${retention.last_cleaned_rows} 条 / 释放 ${formatBytes(retention.last_freed_bytes)}`}
              ；归档失败累计 {retention.archive_failures_total} 次
              {retention.growth.bytes_per_month !== null
                ? `；月增长约 ${formatBytes(Math.max(0, retention.growth.bytes_per_month))}`
                : ""}
            </p>
            {retention.auto_arm.armed ? (
              <p role="status">
                已由阈值自动启用（
                {retention.auto_arm.reason === "size"
                  ? "库 ≥5GB 强提示档"
                  : "事件增长 >500MB/月"}
                ）
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
            <h3>自动备份（D13 / ADR-020）</h3>
            <p>
              {auto.enabled ? "已启用" : "已停用"}；频率：
              {auto.frequency === "weekly" ? "每周" : "每天"}；自动产物保留 {auto.retention} 份
              （手动产物不自动删除）
            </p>
            <p>
              最近运行：{formatTimestamp(auto.last_run_at)}（
              {auto.last_result === "success"
                ? "成功"
                : auto.last_result === "failed"
                  ? `失败，连续 ${auto.consecutive_failures} 次`
                  : "暂无"}
              ）；下次排期：{formatTimestamp(auto.next_run_at)}
            </p>
          </section>
        ) : null}

        <section className="backup-create">
          <h3>手动备份</h3>
          <label>
            标签（可选）
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
              创建备份
            </button>
            <button
              type="button"
              data-testid="backup-create-external"
              disabled={busy}
              onClick={() => void createExternal()}
            >
              备份到外部目录…
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
            备份清单（自动产物保留 {auto?.retention ?? 10} 份；手动产物不自动删除）
          </h3>
          {backups.length === 0 ? (
            <p className="backup-empty" data-testid="backup-empty">
              暂无备份（自动备份默认开启，手动备份随时可创建）
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
                    {new Date(backup.created_at).toLocaleString()}
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
                      恢复
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          )}

          <div className="backup-restore-external">
            <label>
              外部备份文件（.db）
              <input
                type="text"
                data-testid="backup-restore-external"
                value={externalPath}
                placeholder="绝对路径（例如 E:\\backup\\aether-….db）"
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
              使用外部文件恢复
            </button>
          </div>
        </section>

        {pending ? (
          <section className="backup-restore-confirm-section">
            <p className="backup-restore-confirm-note">
              恢复将用所选备份替换当前数据库并重启应用；已确认消息以备份时刻为准。
            </p>
            <button
              type="button"
              data-testid="backup-restore-confirm"
              data-source={pending.source}
              disabled={busy}
              onClick={() => void confirmRestore()}
            >
              确认恢复
            </button>
            <button type="button" disabled={busy} onClick={() => setPending(null)}>
              取消
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
