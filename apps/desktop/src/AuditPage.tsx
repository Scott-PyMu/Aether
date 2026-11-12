/**
 * 审计查询与导出页（M6-03；设计 D9/SE-03、ADR-023、UI-UX §7.3 锚点
 * `audit-page` / `audit-filter` / `audit-export`）。
 *
 * - 覆盖层管理视图（`overlay-audit` + `overlay-back`；不卸载工作台状态）；
 * - 过滤（会话/时间/actor/action/resource）→ `audit_query`（`ts DESC, id DESC`，
 *   分页 `limit=100`）；
 * - 导出（`audit-export`）→ 外部目录可选（复用 `startup_pick_target` 选择器）→
 *   `audit_export` → 回执（`audit-export-result`：`data-result=ok|failed`）。
 *
 * 范围声明（D9 信任级边界）：审计覆盖「经线协议上报的 Agent 操作」与会话/权限/
 * 运行器状态；不覆盖适配器进程内行为。
 */
import { useCallback, useEffect, useState } from "react";

import { auditIpc, type AuditFilters, type AuditIpc, type AuditRow } from "./audit";
import { describeIpcError, ipcErrorCode } from "./startup";
import { t } from "./i18n";

export interface AuditPageProps {
  /** IPC 契约（缺省 = 生产 Tauri 实现；测试注入替身）。 */
  ipc?: AuditIpc;
  /** 返回工作台（`overlay-back`）。 */
  onBack: () => void;
}

/** 审计查询页大小（ADR-023：≤500，缺省 100）。 */
export const AUDIT_PAGE_SIZE = 100;

type ExportOutcome =
  | { result: "ok"; message: string }
  | { result: "failed"; message: string };

/** `datetime-local` 值 → epoch 毫秒（空值/非法为 `undefined`）。 */
export function dateTimeToMs(value: string): number | undefined {
  if (!value) {
    return undefined;
  }
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? undefined : parsed;
}

function formatTs(ts: number): string {
  const date = new Date(ts);
  return Number.isNaN(date.getTime()) ? String(ts) : date.toISOString();
}

export function AuditPage({ ipc = auditIpc, onBack }: AuditPageProps) {
  const [sessionId, setSessionId] = useState("");
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const [resource, setResource] = useState("");
  const [fromTs, setFromTs] = useState("");
  const [toTs, setToTs] = useState("");
  const [rows, setRows] = useState<AuditRow[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [offset, setOffset] = useState(0);
  const [busy, setBusy] = useState(false);
  const [target, setTarget] = useState("");
  const [outcome, setOutcome] = useState<ExportOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);

  const collectFilters = useCallback(
    (): AuditFilters => ({
      sessionId: sessionId.trim() || undefined,
      actor: actor || undefined,
      action: action.trim() || undefined,
      resource: resource.trim() || undefined,
      fromTs: dateTimeToMs(fromTs),
      toTs: dateTimeToMs(toTs),
    }),
    [action, actor, fromTs, resource, sessionId, toTs],
  );

  const load = useCallback(
    async (filters: AuditFilters, nextOffset: number) => {
      setBusy(true);
      setError(null);
      setErrorCode(null);
      try {
        const response = await ipc.query(filters, {
          limit: AUDIT_PAGE_SIZE,
          offset: nextOffset,
        });
        setRows(response.rows);
        setHasMore(response.has_more);
        setOffset(response.offset);
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  // 首屏查询一次；过滤条件变更不自动触发（经「查询」按钮显式提交）。
  useEffect(() => {
    void load({}, 0);
  }, [load]);

  const pickTarget = useCallback(async () => {
    setError(null);
    setErrorCode(null);
    try {
      const picked = await ipc.pickTargetDir();
      if (picked === null) {
        return;
      }
      setTarget(picked);
      setOutcome(null);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [ipc]);

  const exportAudit = useCallback(async () => {
    setBusy(true);
    setError(null);
    setErrorCode(null);
    try {
      const response = await ipc.exportAudit(collectFilters(), target.trim() || null);
      const report = response.export;
      setOutcome({
        result: "ok",
        message:
          t("audit.audit_exported", { p0: report.path }) +
          t("audit.rows_bytes_sha256", { p0: report.rows, p1: report.bytes, p2: report.sha256.slice(0, 12) }),
      });
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
      setOutcome({ result: "failed", message: describeIpcError(failure) });
    } finally {
      setBusy(false);
    }
  }, [collectFilters, ipc, target]);

  const hasPrev = offset > 0;
  const hasNext = hasMore;

  return (
    <div className="overlay" data-testid="overlay-audit">
      <section
        className="audit-page"
        data-testid="audit-page"
        role="dialog"
        aria-modal="true"
      >
        <header className="audit-header">
          <h2>{t("audit.audit")}</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            {t("app.action.back_to_workbench")}
          </button>
        </header>

        <p className="audit-note">
          {t("audit.audit_covers_agent_operations_reported")}
        </p>

        <form
          className="audit-filter"
          data-testid="audit-filter"
          onSubmit={(event) => {
            event.preventDefault();
            void load(collectFilters(), 0);
          }}
        >
          <label>
            {t("audit.session_id")}
            <input
              type="text"
              data-testid="audit-filter-session"
              value={sessionId}
              placeholder={t("audit.26_character_ulid_no_filter")}
              onChange={(event) => setSessionId(event.target.value)}
            />
          </label>
          <label>
            {t("audit.actor_label")}
            <select
              data-testid="audit-filter-actor"
              value={actor}
              onChange={(event) => setActor(event.target.value)}
            >
              <option value="">{t("audit.all")}</option>
              <option value="user">{t("audit.actor_user")}</option>
              <option value="agent">{t("audit.actor_agent")}</option>
              <option value="system">{t("audit.actor_system")}</option>
            </select>
          </label>
          <label>
            {t("audit.action_label")}
            <input
              type="text"
              data-testid="audit-filter-action"
              value={action}
              placeholder={t("audit.e_g_tool_call_session")}
              onChange={(event) => setAction(event.target.value)}
            />
          </label>
          <label>
            {t("audit.resource_label")}
            <input
              type="text"
              data-testid="audit-filter-resource"
              value={resource}
              placeholder={t("audit.e_g_mock_echo_runtime")}
              onChange={(event) => setResource(event.target.value)}
            />
          </label>
          <label>
            {t("audit.start_time")}
            <input
              type="datetime-local"
              data-testid="audit-filter-from"
              value={fromTs}
              onChange={(event) => setFromTs(event.target.value)}
            />
          </label>
          <label>
            {t("audit.end_time")}
            <input
              type="datetime-local"
              data-testid="audit-filter-to"
              value={toTs}
              onChange={(event) => setToTs(event.target.value)}
            />
          </label>
          <button type="submit" data-testid="audit-query" disabled={busy}>
            {t("audit.query")}
          </button>
        </form>

        <p className="audit-summary" data-testid="audit-summary">
          {t("audit.page")} {rows.length === 0 ? 0 : offset + 1}–{offset + rows.length} {t("app.common.items")}
          {hasMore ? t("audit.more_available") : t("audit.last_page")}
        </p>

        <table className="audit-rows" data-testid="audit-rows">
          <thead>
            <tr>
              <th>{t("audit.time")}</th>
              <th>{t("audit.actor_label")}</th>
              <th>{t("audit.action_label")}</th>
              <th>{t("audit.resource_label")}</th>
              <th>{t("audit.result_label")}</th>
              <th>{t("audit.detail_label")}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.id} data-testid="audit-row" data-row-id={row.id}>
                <td>{formatTs(row.ts)}</td>
                <td>{row.actor}</td>
                <td>{row.action}</td>
                <td>{row.resource ?? ""}</td>
                <td>{row.result ?? ""}</td>
                <td className="audit-detail">{row.detail ?? ""}</td>
              </tr>
            ))}
          </tbody>
        </table>

        <div className="audit-pagination">
          <button
            type="button"
            data-testid="audit-prev"
            disabled={busy || !hasPrev}
            onClick={() => void load(collectFilters(), Math.max(0, offset - AUDIT_PAGE_SIZE))}
          >
            {t("audit.previous")}
          </button>
          <button
            type="button"
            data-testid="audit-next"
            disabled={busy || !hasNext}
            onClick={() => void load(collectFilters(), offset + AUDIT_PAGE_SIZE)}
          >
            {t("audit.next")}
          </button>
        </div>

        <section className="audit-target-section">
          <label>
            {t("audit.export_directory_external_path_chosen")}
            <input
              type="text"
              data-testid="audit-target"
              value={target}
              readOnly
              placeholder={t("audit.click_choose_directory_or_export")}
            />
          </label>
          <div className="audit-actions">
            <button
              type="button"
              data-testid="audit-pick"
              disabled={busy}
              onClick={() => void pickTarget()}
            >
              {t("app.action.choose_directory")}
            </button>
            <button
              type="button"
              data-testid="audit-export"
              disabled={busy}
              onClick={() => void exportAudit()}
            >
              {t("audit.export_audit_jsonl")}
            </button>
          </div>
          <p className="audit-note">
            {t("audit.export_as_jsonl_manifest_row")}
          </p>
        </section>

        {outcome ? (
          <p
            className="audit-export-result"
            data-testid="audit-export-result"
            data-result={outcome.result}
            role={outcome.result === "failed" ? "alert" : undefined}
          >
            {outcome.message}
          </p>
        ) : null}

        {error ? (
          <p
            className="audit-error"
            data-testid="audit-error"
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
