/**
 * 会话工作台（M3-02；UI-01 单窗口 + 会话切换、UI-02 Agent 选择器、UI-05 会话级模型）。
 *
 * 组成：
 * - 会话列表 / 切换（左侧栏）；
 * - 新建会话（运行时选择器 + 会话级模型输入；`session_create` 参数透传）；
 * - 消息流（虚拟滚动 + Markdown + 代码高亮；事件流实时投影）；
 * - 状态条（会话状态 / run 状态 / 中断入口）；
 * - 输入区（发送 → `session_send` ack 快路径 → 乐观用户气泡）。
 *
 * 依赖注入：`store`（EventStore，默认 `appEventStore`）与 `ipc`（会话命令面，默认
 * Tauri 生产实现）——E2E/单测注入替身即可覆盖完整交互路径。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { appEventStore } from "./aetherStore";
import { describeIpcError } from "./startup";
import type { EventStore } from "./eventStore";
import { HistoryOverflowNotice } from "./HistoryOverflowNotice";
import { Markdown } from "./markdown";
import {
  RUNTIME_STATUS_LABELS,
  SESSION_STATUS_LABELS,
  sessionIpc,
  type MessageRow,
  type RuntimeInfo,
  type SessionIpc,
  type SessionSummary,
} from "./session";
import { projectSession } from "./sessionProjection";
import { generateUlid } from "./ulid";
import { useSessionEvents } from "./useSessionEvents";
import { VirtualList } from "./VirtualList";

/** 历史消息加载条数（`messages_page` 最近一页；与 EventStore 重载默认一致）。 */
export const DEFAULT_HISTORY_LIMIT = 500;
/** 虚拟列表行高 / 视口（像素；消息气泡按内容高度渲染在行内）。 */
export const STREAM_ITEM_HEIGHT = 96;
export const STREAM_VIEWPORT_HEIGHT = 480;
/** 空会话占位 id（未选会话时 hook 仍需一个稳定键）。 */
const NO_SESSION = "__none__";

export interface SessionWorkbenchProps {
  store?: EventStore;
  ipc?: SessionIpc;
  historyLimit?: number;
  streamItemHeight?: number;
  streamHeight?: number;
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

export function SessionWorkbench({
  store = appEventStore,
  ipc = sessionIpc,
  historyLimit = DEFAULT_HISTORY_LIMIT,
  streamItemHeight = STREAM_ITEM_HEIGHT,
  streamHeight = STREAM_VIEWPORT_HEIGHT,
}: SessionWorkbenchProps) {
  const [runtimes, setRuntimes] = useState<RuntimeInfo[]>([]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<MessageRow[]>([]);
  const [selectedRuntime, setSelectedRuntime] = useState<string>("");
  const [modelDraft, setModelDraft] = useState("");
  const [titleDraft, setTitleDraft] = useState("");
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastAck, setLastAck] = useState<{ runId: string; duplicate: boolean } | null>(null);
  /** T1 埋点：会话创建（点击 → `session_create` ack）耗时（毫秒）。 */
  const [createLatencyMs, setCreateLatencyMs] = useState<number | null>(null);
  const sendInFlight = useRef(false);

  const eventsState = useSessionEvents(store, activeSessionId ?? NO_SESSION);
  const projection = useMemo(
    () => projectSession(eventsState.events, messages),
    [eventsState.events, messages],
  );
  const activeSession = useMemo(
    () => sessions.find((session) => session.id === activeSessionId) ?? null,
    [sessions, activeSessionId],
  );

  const refreshRuntimes = useCallback(async () => {
    try {
      const list = await ipc.listRuntimes();
      setRuntimes(list);
      setSelectedRuntime((current) => {
        if (current && list.some((runtime) => runtime.id === current)) {
          return current;
        }
        const firstReady = list.find((runtime) => runtime.status === "ready") ?? list[0];
        return firstReady?.id ?? "";
      });
    } catch (failure) {
      setError(describeIpcError(failure));
    }
  }, [ipc]);

  const refreshSessions = useCallback(async () => {
    try {
      const list = await ipc.listSessions();
      setSessions(list);
      setActiveSessionId((current) => {
        if (current && list.some((session) => session.id === current)) {
          return current;
        }
        return list[0]?.id ?? null;
      });
    } catch (failure) {
      setError(describeIpcError(failure));
    }
  }, [ipc]);

  const loadMessages = useCallback(
    async (sessionId: string) => {
      try {
        const page = await ipc.messagesPage({ session_id: sessionId, limit: historyLimit });
        setMessages((previous) => mergeMessages(previous, page.messages ?? []));
      } catch (failure) {
        setError(describeIpcError(failure));
      }
    },
    [ipc, historyLimit],
  );

  useEffect(() => {
    void refreshRuntimes();
    void refreshSessions();
  }, [refreshRuntimes, refreshSessions]);

  useEffect(() => {
    if (!activeSessionId) {
      return;
    }
    void loadMessages(activeSessionId);
  }, [activeSessionId, loadMessages]);

  const onCreateSession = useCallback(async () => {
    if (!selectedRuntime) {
      setError("请选择运行时");
      return;
    }
    const title = titleDraft.trim() || `会话 ${new Date().toLocaleTimeString()}`;
    const model = modelDraft.trim();
    const startedAt = performance.now();
    try {
      const created = await ipc.createSession({
        runtime_id: selectedRuntime,
        title,
        ...(model ? { model } : {}),
      });
      // T1 埋点（M4-04 验收读取）：会话创建 P50<500ms / P95<2s 的 UI 侧锚点。
      setCreateLatencyMs(Math.round(performance.now() - startedAt));
      setTitleDraft("");
      // 立即入列表并选中（不依赖列表刷新时序）；随后以 `session_list` 为准刷新。
      setSessions((previous) =>
        previous.some((session) => session.id === created.id)
          ? previous
          : [created, ...previous],
      );
      setActiveSessionId(created.id);
      await refreshSessions();
    } catch (failure) {
      setError(describeIpcError(failure));
    }
  }, [ipc, modelDraft, refreshSessions, selectedRuntime, titleDraft]);

  const onSend = useCallback(async () => {
    const text = draft.trim();
    if (!activeSessionId || text.length === 0 || sendInFlight.current) {
      return;
    }
    sendInFlight.current = true;
    setSending(true);
    setError(null);
    try {
      const ack = await ipc.sendMessage({
        session_id: activeSessionId,
        text,
        client_msg_id: generateUlid(),
      });
      setDraft("");
      setLastAck({ runId: ack.run_id, duplicate: ack.duplicate });
      if (!ack.duplicate) {
        // 乐观用户气泡（发送即展示；历史由 messages_page 基线回填）。
        const optimistic: MessageRow = {
          id: ack.message_id,
          session_id: activeSessionId,
          run_id: ack.run_id,
          client_msg_id: null,
          role: "user",
          content: text,
          seq: Number.MAX_SAFE_INTEGER,
          created_at: Date.now(),
        };
        setMessages((previous) => mergeMessages(previous, [optimistic]));
      }
      await refreshSessions();
    } catch (failure) {
      setError(describeIpcError(failure));
    } finally {
      sendInFlight.current = false;
      setSending(false);
    }
  }, [activeSessionId, draft, ipc, refreshSessions]);

  const onInterrupt = useCallback(async () => {
    if (!activeSessionId) {
      return;
    }
    try {
      await ipc.interruptSession(activeSessionId);
      await refreshSessions();
    } catch (failure) {
      setError(describeIpcError(failure));
    }
  }, [activeSessionId, ipc, refreshSessions]);

  const interruptible =
    projection.activeRunId !== null || activeSession?.status === "running";

  return (
    <section className="workbench" data-testid="workbench">
      {/* T1 埋点锚点（M4-04 读取；`data-ms` 为空表示尚未创建会话）。 */}
      <span
        hidden
        data-testid="create-latency-ms"
        data-ms={createLatencyMs === null ? "" : String(createLatencyMs)}
      />
      <aside className="workbench-sidebar">
        <h2 className="workbench-title">会话</h2>
        <ul className="session-list" data-testid="session-list">
          {sessions.map((session) => (
            <li key={session.id}>
              <button
                type="button"
                data-testid="session-item"
                data-session-id={session.id}
                data-active={String(session.id === activeSessionId)}
                className={session.id === activeSessionId ? "session-item active" : "session-item"}
                onClick={() => setActiveSessionId(session.id)}
              >
                <span className="session-item-title">{session.title}</span>
                <span className="session-item-status" data-testid="session-item-status">
                  {SESSION_STATUS_LABELS[session.status]}
                </span>
                {session.model ? (
                  <span className="session-item-model" data-testid="session-item-model">
                    {session.model}
                  </span>
                ) : null}
              </button>
            </li>
          ))}
          {sessions.length === 0 ? (
            <li className="session-empty" data-testid="session-list-empty">
              暂无会话
            </li>
          ) : null}
        </ul>

        <h2 className="workbench-title">运行时</h2>
        <div className="runtime-selector" data-testid="runtime-selector" role="radiogroup">
          {runtimes.map((runtime) => (
            <button
              key={runtime.id}
              type="button"
              role="radio"
              aria-checked={runtime.id === selectedRuntime}
              data-testid="runtime-option"
              data-runtime-id={runtime.id}
              data-status={runtime.status}
              data-selected={String(runtime.id === selectedRuntime)}
              className={
                runtime.id === selectedRuntime ? "runtime-option selected" : "runtime-option"
              }
              onClick={() => setSelectedRuntime(runtime.id)}
            >
              <span className="runtime-name">{runtime.name}</span>
              <span className="runtime-status">{RUNTIME_STATUS_LABELS[runtime.status]}</span>
              {runtime.status_reason ? (
                <span className="runtime-reason" data-testid="runtime-reason">
                  {runtime.status_reason}
                </span>
              ) : null}
              <span className="runtime-capabilities" data-testid="runtime-capabilities">
                {runtime.capabilities.map((capability) => (
                  <span
                    key={capability}
                    className="capability-badge"
                    data-testid="capability-badge"
                  >
                    {capability}
                  </span>
                ))}
              </span>
            </button>
          ))}
          {runtimes.length === 0 ? (
            <p className="runtime-empty" data-testid="runtime-list-empty">
              无可用运行时（适配器注册随打包里程碑落地）
            </p>
          ) : null}
        </div>

        <div className="session-create" data-testid="session-create-form">
          <input
            type="text"
            data-testid="session-title-input"
            placeholder="会话标题"
            value={titleDraft}
            onChange={(event) => setTitleDraft(event.target.value)}
          />
          <input
            type="text"
            data-testid="session-model-input"
            placeholder="会话级模型（可选，覆盖全局默认）"
            value={modelDraft}
            onChange={(event) => setModelDraft(event.target.value)}
          />
          <button type="button" data-testid="session-create-submit" onClick={onCreateSession}>
            新建会话
          </button>
        </div>
      </aside>

      <div className="workbench-main">
        {activeSession ? (
          <header className="session-status-bar" data-testid="status-bar">
            <span data-testid="session-status" data-status={activeSession.status}>
              {SESSION_STATUS_LABELS[activeSession.status]}
            </span>
            <span data-testid="run-status" data-run-id={projection.activeRunId ?? ""}>
              {projection.activeRunId
                ? "run 运行中"
                : (projection.runs.at(-1)?.status ?? "无进行中 run")}
            </span>
            <button
              type="button"
              data-testid="interrupt"
              disabled={!interruptible}
              onClick={onInterrupt}
            >
              中断
            </button>
            {lastAck ? (
              <span className="last-ack" data-testid="last-ack" data-run-id={lastAck.runId}>
                {lastAck.duplicate ? "幂等命中" : "已受理"} {lastAck.runId.slice(-6)}
              </span>
            ) : null}
          </header>
        ) : (
          <p className="workbench-hint" data-testid="no-session-hint">
            请选择或新建会话
          </p>
        )}

        {activeSessionId ? (
          <HistoryOverflowNotice
            store={store}
            sessionId={activeSessionId}
            onReload={() => {
              void loadMessages(activeSessionId);
            }}
          />
        ) : null}

        <div className="message-stream" data-testid="message-stream">
          <VirtualList
            items={projection.bubbles}
            itemHeight={streamItemHeight}
            height={streamHeight}
            testId="message-list"
            className="message-list"
            itemKey={(bubble) => bubble.key}
            renderItem={(bubble) => (
              <article
                className={`message-bubble message-${bubble.role}`}
                data-testid="message-bubble"
                data-role={bubble.role}
                data-streaming={String(bubble.streaming)}
                data-run-id={bubble.runId ?? ""}
                data-key={bubble.key}
              >
                <span className="message-role">
                  {bubble.role === "user" ? "你" : "助手"}
                </span>
                <div className="message-content">
                  {bubble.role === "user" ? (
                    // 用户输入按纯文本渲染（不解释 Markdown；预格式保留换行）。
                    <span className="message-plain">{bubble.text}</span>
                  ) : (
                    <Markdown text={bubble.text} />
                  )}
                </div>
                {bubble.streaming ? (
                  <span className="message-streaming" data-testid="streaming-indicator">
                    生成中…
                  </span>
                ) : null}
              </article>
            )}
          />
          {projection.bubbles.length === 0 ? (
            <p className="stream-empty" data-testid="message-stream-empty">
              暂无消息
            </p>
          ) : null}
        </div>

        {projection.toolCalls.length > 0 ? (
          <ul className="tool-calls" data-testid="tool-calls">
            {projection.toolCalls.map((tool) => (
              <li
                key={tool.id}
                data-testid="tool-call"
                data-tool-name={tool.name}
                data-tool-status={tool.status}
              >
                {tool.name}：{tool.status}
                {tool.errorCode ? `（${tool.errorCode}）` : ""}
              </li>
            ))}
          </ul>
        ) : null}

        <div className="composer" data-testid="composer">
          <textarea
            data-testid="composer-input"
            placeholder="输入消息（Ctrl+Enter 发送）"
            value={draft}
            disabled={!activeSessionId}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
                event.preventDefault();
                void onSend();
              }
            }}
          />
          <button
            type="button"
            data-testid="composer-send"
            disabled={!activeSessionId || sending || draft.trim().length === 0}
            onClick={() => void onSend()}
          >
            发送
          </button>
        </div>

        {error ? (
          <p className="workbench-error" data-testid="workbench-error" role="alert">
            {error}
          </p>
        ) : null}
      </div>
    </section>
  );
}
