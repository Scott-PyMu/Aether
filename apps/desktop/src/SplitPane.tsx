/**
 * 分屏第二窗格（M7-04；UI-01 完整、设计 §2.3「多视图」P1）。
 *
 * 口径：
 * - **视图隔离（DoD1）**：本窗格只订阅自己的会话视图（`useSessionEvents(store, session.id)`）
 *   ——EventStore 按 `(session_id, seq)` 分会话存储，另一窗格/后台标签的事件不会串流；
 * - 消息基线经 `messages_page` 独立加载，与主窗格互不影响；
 * - 发送经 `session_send`（与主窗格同命令面；成功后刷新会话列表以更新标签角标）；
 * - 降级（`persist_degraded`）/ `paused` 会话禁用输入（与主窗格同一禁用矩阵）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { EventStore } from "./eventStore";
import { Markdown } from "./markdown";
import type { MessageRow, SessionIpc, SessionSummary } from "./session";
import { SESSION_STATUS_LABELS, THINKING_DEPTH_DEFAULT } from "./session";
import { projectSession } from "./sessionProjection";
import { generateUlid } from "./ulid";
import { useSessionEvents } from "./useSessionEvents";
import { VirtualList } from "./VirtualList";

export interface SplitPaneProps {
  store: EventStore;
  ipc: SessionIpc;
  /** 分屏窗格会话（调用方保证非空且与主窗格不同）。 */
  session: SessionSummary;
  historyLimit: number;
  streamItemHeight: number;
  streamHeight: number;
  /** 存储降级（只读）→ 输入禁用。 */
  degraded: boolean;
  /** 失败上报（工作台 `workbench-error` + `data-code`）。 */
  onError: (failure: unknown) => void;
  /** 发送成功后刷新会话列表（标签角标/状态一致）。 */
  onRefreshSessions: () => Promise<void>;
}

function mergeMessages(previous: MessageRow[], incoming: MessageRow[]): MessageRow[] {
  const merged = new Map<string, MessageRow>();
  for (const message of previous) {
    merged.set(message.id, message);
  }
  for (const message of incoming) {
    merged.set(message.id, message);
  }
  return [...merged.values()].sort((left, right) => left.created_at - right.created_at);
}

export function SplitPane({
  store,
  ipc,
  session,
  historyLimit,
  streamItemHeight,
  streamHeight,
  degraded,
  onError,
  onRefreshSessions,
}: SplitPaneProps) {
  const sessionId = session.id;
  const [messages, setMessages] = useState<MessageRow[]>([]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);

  // 视图隔离：仅订阅本窗格会话的事件视图（DoD1）。
  const eventsState = useSessionEvents(store, sessionId);
  const projection = useMemo(
    () => projectSession(eventsState.events, messages),
    [eventsState.events, messages],
  );

  useEffect(() => {
    let active = true;
    ipc
      .messagesPage({ session_id: sessionId, limit: historyLimit })
      .then((page) => {
        if (active) {
          setMessages((previous) => mergeMessages(previous, page.messages ?? []));
        }
      })
      .catch((failure: unknown) => {
        if (active) {
          onError(failure);
        }
      });
    return () => {
      active = false;
    };
  }, [historyLimit, ipc, onError, sessionId]);

  const onSend = useCallback(async () => {
    const text = draft.trim();
    if (text.length === 0 || sending || degraded || session.status === "paused") {
      return;
    }
    setSending(true);
    try {
      const ack = await ipc.sendMessage({
        session_id: sessionId,
        text,
        client_msg_id: generateUlid(),
        thinking_depth: session.thinking_depth ?? THINKING_DEPTH_DEFAULT,
      });
      setDraft("");
      if (!ack.duplicate) {
        const optimistic: MessageRow = {
          id: ack.message_id,
          session_id: sessionId,
          run_id: ack.run_id,
          client_msg_id: null,
          role: "user",
          content: text,
          seq: Number.MAX_SAFE_INTEGER,
          created_at: Date.now(),
        };
        setMessages((previous) => mergeMessages(previous, [optimistic]));
      }
      await onRefreshSessions();
    } catch (failure) {
      onError(failure);
    } finally {
      setSending(false);
    }
  }, [degraded, draft, ipc, onError, onRefreshSessions, sending, session, sessionId]);

  const sendDisabled =
    degraded ||
    sending ||
    draft.trim().length === 0 ||
    session.status === "paused";

  return (
    <section
      className="split-pane"
      data-testid="split-pane"
      data-session-id={sessionId}
      data-status={session.status}
      aria-label={`分屏窗格：${session.title}`}
    >
      <header className="split-pane-header">
        <span className="split-pane-title" data-testid="split-pane-title">
          {session.title}
        </span>
        <span
          className="split-pane-status"
          data-testid="split-pane-status"
          data-status={session.status}
        >
          {SESSION_STATUS_LABELS[session.status]}
        </span>
      </header>
      <div className="message-stream split-message-stream" data-testid="split-message-stream">
        <VirtualList
          items={projection.bubbles}
          itemHeight={streamItemHeight}
          height={streamHeight}
          testId="split-message-list"
          className="message-list"
          itemKey={(bubble) => bubble.key}
          renderItem={(bubble) => (
            <article
              className={`message-bubble message-${bubble.role}`}
              data-testid="split-message-bubble"
              data-role={bubble.role}
              data-streaming={String(bubble.streaming)}
              data-run-id={bubble.runId ?? ""}
              data-session-id={sessionId}
              data-key={bubble.key}
            >
              <span className="message-role">{bubble.role === "user" ? "你" : "助手"}</span>
              <div className="message-content">
                {bubble.role === "user" ? (
                  <span className="message-plain">{bubble.text}</span>
                ) : (
                  <Markdown text={bubble.text} />
                )}
              </div>
              {bubble.streaming ? (
                <span className="message-streaming" data-testid="split-streaming-indicator">
                  生成中…
                </span>
              ) : null}
            </article>
          )}
        />
        {projection.bubbles.length === 0 ? (
          <p className="stream-empty" data-testid="split-message-stream-empty">
            暂无消息
          </p>
        ) : null}
      </div>
      <div className="composer split-composer" data-testid="split-composer">
        <textarea
          data-testid="split-composer-input"
          placeholder="在分屏窗格输入消息（Ctrl+Enter 发送）"
          value={draft}
          disabled={degraded || session.status === "paused"}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
              event.preventDefault();
              void onSend();
            }
          }}
        />
        <div className="composer-toolbar">
          <button
            type="button"
            data-testid="split-composer-send"
            disabled={sendDisabled}
            onClick={() => void onSend()}
          >
            发送
          </button>
        </div>
        {session.status === "paused" ? (
          <p className="composer-degraded-hint" role="status">
            会话已暂停：恢复后发送解除（`session_paused`）。
          </p>
        ) : null}
      </div>
    </section>
  );
}
