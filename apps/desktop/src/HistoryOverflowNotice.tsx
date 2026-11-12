/**
 * 「历史消息过多」提示（M3-01 DoD2；D4 失败场景表）。
 *
 * 缺口 >10k 时显示；用户确认后清空当前会话 EventStore 缓存，按 `last_seq`
 * 分页加载最近 N 条（默认 500，可配置）——**不重启核心、不重启应用**。
 */
import type { EventStore } from "./eventStore";
import { useSessionEvents } from "./useSessionEvents";
import { t } from "./i18n";

export function HistoryOverflowNotice({
  store,
  sessionId,
  onReload,
}: {
  store: EventStore;
  sessionId: string;
  /** 重载完成后回调（M3-02 工作台：同步重载消息基线）。 */
  onReload?: () => void;
}) {
  const { historyTooLarge, reloadPending, reloadLimit, error } =
    useSessionEvents(store, sessionId);

  if (!historyTooLarge) {
    return null;
  }

  return (
    <section
      className="history-overflow"
      data-testid="history-overflow"
      role="alert"
    >
      <p className="history-overflow-title">
        {t("session.history.too_many_historical_messages_close")}
      </p>
      <p className="history-overflow-detail">
        {t("session.history.gap_exceeds_10k_d4_refuses")} {reloadLimit}{" "}
        {t("session.history.messages_no_core_or_app")}
      </p>
      <button
        type="button"
        data-testid="history-reload"
        disabled={reloadPending}
        onClick={() => {
          void store.confirmReload(sessionId).then(() => {
            onReload?.();
          });
        }}
      >
        {t("session.history.reload_the_latest")} {reloadLimit} {t("app.common.items")}
      </button>
      {error ? (
        <p className="history-overflow-error" data-testid="history-reload-error">
          {error}
        </p>
      ) : null}
    </section>
  );
}
