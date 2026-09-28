/**
 * 会话工作台（M3-02；M3-03 集成权限中心与运行状态面板）。
 *
 * 组成：
 * - 顶栏（TopBar；Q8：合并原状态条 + 运行时徽标/存储指示/待审批计数）；
 * - 会话列表 / 切换、新建会话（左侧栏）；
 * - 消息流（虚拟滚动 + Markdown + 代码高亮；事件流实时投影）；
 * - 输入区（发送 → `session_send` ack 快路径 → 乐观用户气泡）；
 * - 右栏（RuntimePanel S-04 + PermissionPanel S-03；<1280px 折叠为抽屉）。
 *
 * 依赖注入：`store`（EventStore，默认 `appEventStore`）、`ipc`（会话命令面，默认
 * Tauri 生产实现）与 `permissionIpc`（权限/运行时控制命令面，默认 Tauri 生产实现）
 * ——E2E/单测注入替身即可覆盖完整交互路径。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { appEventStore } from "./aetherStore";
import { describeIpcError, ipcErrorCode } from "./startup";
import type { EventStore } from "./eventStore";
import { STORAGE_L2_THRESHOLD } from "./health";
import { useStorageHealth } from "./healthBus";
import { HistoryOverflowNotice } from "./HistoryOverflowNotice";
import { Markdown } from "./markdown";
import { permissionIpc as productionPermissionIpc, type PermissionIpc } from "./permission";
import { PermissionPanel } from "./PermissionPanel";
import { RuntimePanel } from "./RuntimePanel";
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
import { TopBar } from "./TopBar";
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
  permissionIpc?: PermissionIpc;
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
  permissionIpc = productionPermissionIpc,
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
  /** 结构化错误码（UI-UX §7.3 通用行：错误元素补 `data-code`；无码为 `null`）。 */
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [lastAck, setLastAck] = useState<{ runId: string; duplicate: boolean } | null>(null);
  /** T1 埋点：会话创建（点击 → `session_create` ack）耗时（毫秒）。 */
  const [createLatencyMs, setCreateLatencyMs] = useState<number | null>(null);
  /** M3-06：重放进行中的 run（按钮去抖）。 */
  const [retryingRunId, setRetryingRunId] = useState<string | null>(null);
  /** M3-03：右栏（运行时/权限）抽屉开关（<1280px 断点；≥1280 常驻）。 */
  const [rightOpen, setRightOpen] = useState(true);
  /** M3-03：待审批计数（全局面板数据源 → 顶栏徽标）。 */
  const [pendingPermissionCount, setPendingPermissionCount] = useState(0);
  const sendInFlight = useRef(false);

  // M3-06：存储降级（只读）联动——发送入口禁用；修复 + 重启前不得恢复。
  const storageHealth = useStorageHealth();
  const degraded = storageHealth.status === "degraded";
  // M3-06 `storage-backpressure-notice`（D8/ADR-004 临时背压，与 persist_degraded 区分）：
  // - scope=run：写队列 > L2（4096）→ 新 run 被拒，队列回落（≤1024）自动解除；
  // - scope=adapter：适配器 `degraded + status_reason=storage_backpressure` 隔离中，
  //   重启后自动解除。仅提示，不改变发送入口（拒绝由核心准入执行）。
  const queueDepth = storageHealth.report?.write_queue_depth ?? 0;
  const queueBackpressured = !degraded && queueDepth > STORAGE_L2_THRESHOLD;
  const isolatedRuntime = (storageHealth.report?.runtimes ?? []).find(
    (runtime) => runtime.status === "degraded" && runtime.status_reason === "storage_backpressure",
  );
  const backpressureScope = queueBackpressured
    ? "run"
    : isolatedRuntime
      ? "adapter"
      : null;

  const eventsState = useSessionEvents(store, activeSessionId ?? NO_SESSION);
  const projection = useMemo(
    () => projectSession(eventsState.events, messages),
    [eventsState.events, messages],
  );
  const activeSession = useMemo(
    () => sessions.find((session) => session.id === activeSessionId) ?? null,
    [sessions, activeSessionId],
  );
  /** M3-06：run 视图索引（消息气泡 → 终态/取消原因 → 重试入口）。 */
  const runViews = useMemo(
    () => new Map(projection.runs.map((run) => [run.runId, run])),
    [projection.runs],
  );

  /** 结构化错误上报（展示文本 + `data-code`；UI-UX §7.3）。 */
  const reportError = useCallback((failure: unknown) => {
    setError(describeIpcError(failure));
    setErrorCode(ipcErrorCode(failure));
  }, []);
  const clearError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  const refreshRuntimes = useCallback(async () => {
    try {
      const list = await ipc.listRuntimes();
      setRuntimes(list);
      setSelectedRuntime((current) => {
        if (current && list.some((runtime) => runtime.id === current)) {
          return current;
        }
        // M3-03 DoD3：`disabled` 运行时不可选（不可创建会话）；默认优先 ready。
        const firstReady =
          list.find((runtime) => runtime.status === "ready") ??
          list.find((runtime) => runtime.status !== "disabled");
        return firstReady?.id ?? "";
      });
    } catch (failure) {
      reportError(failure);
    }
  }, [ipc, reportError]);

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
      reportError(failure);
    }
  }, [ipc, reportError]);

  const loadMessages = useCallback(
    async (sessionId: string) => {
      try {
        const page = await ipc.messagesPage({ session_id: sessionId, limit: historyLimit });
        setMessages((previous) => mergeMessages(previous, page.messages ?? []));
      } catch (failure) {
        reportError(failure);
      }
    },
    [ipc, historyLimit, reportError],
  );

  useEffect(() => {
    void refreshRuntimes();
    void refreshSessions();
  }, [refreshRuntimes, refreshSessions]);

  // M3-03：运行时状态随既有 `health` 轮询（5s）变化刷新注册表快照——不新增轮询循环，
  // 仅在摘要签名（id/status/reason）变化时重取（disabled/degraded 徽标与恢复入口联动）。
  // `health.runtimes` 为数组（含 `[]`，监督器已接线）时附加就绪标记，覆盖启动过渡窗口
  // 首次 `runtimes_list` 失败后的恢复重取。
  const runtimeSummaries = storageHealth.report?.runtimes ?? null;
  const runtimeSummarySignature =
    runtimeSummaries === null
      ? ""
      : `${runtimeSummaries
          .map((runtime) => `${runtime.id}:${runtime.status}:${runtime.status_reason ?? ""}`)
          .join("|")}#wired`;
  const lastRuntimeSignature = useRef("");
  useEffect(() => {
    if (
      runtimeSummarySignature === "" ||
      lastRuntimeSignature.current === runtimeSummarySignature
    ) {
      return;
    }
    lastRuntimeSignature.current = runtimeSummarySignature;
    void refreshRuntimes();
  }, [refreshRuntimes, runtimeSummarySignature]);

  useEffect(() => {
    if (!activeSessionId) {
      return;
    }
    void loadMessages(activeSessionId);
  }, [activeSessionId, loadMessages]);

  const onCreateSession = useCallback(async () => {
    if (!selectedRuntime) {
      setErrorCode(null);
      setError("请选择运行时");
      return;
    }
    // M3-03 DoD3（D5）：`disabled` 运行时不可创建会话（选择器已禁用；此处兜底
    // 覆盖「选中后运行时转为 disabled」的窗口；后端另有等价防线）。
    const selected = runtimes.find((runtime) => runtime.id === selectedRuntime);
    if (selected?.status === "disabled") {
      const reason = selected.status_reason ?? "disabled";
      setErrorCode("invalid_value");
      setError(`运行时 ${selected.name} 已禁用（${reason}），不可创建会话；请先修复并重新启用`);
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
      reportError(failure);
    }
  }, [ipc, modelDraft, refreshSessions, reportError, runtimes, selectedRuntime, titleDraft]);

  const onSend = useCallback(async () => {
    const text = draft.trim();
    if (!activeSessionId || text.length === 0 || sendInFlight.current) {
      return;
    }
    sendInFlight.current = true;
    setSending(true);
    clearError();
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
      reportError(failure);
    } finally {
      sendInFlight.current = false;
      setSending(false);
    }
  }, [activeSessionId, clearError, draft, ipc, refreshSessions, reportError]);

  const onInterrupt = useCallback(async () => {
    if (!activeSessionId) {
      return;
    }
    try {
      await ipc.interruptSession(activeSessionId);
      await refreshSessions();
    } catch (failure) {
      reportError(failure);
    }
  }, [activeSessionId, ipc, refreshSessions, reportError]);

  /** M3-06：一键重放（`run_retry`；仅终态 run 由后端判定；新 run 事件流驱动渲染）。 */
  const onRetryRun = useCallback(
    async (runId: string) => {
      if (retryingRunId !== null) {
        return;
      }
      setRetryingRunId(runId);
      clearError();
      try {
        await ipc.retryRun(runId);
        await refreshSessions();
      } catch (failure) {
        reportError(failure);
      } finally {
        setRetryingRunId(null);
      }
    },
    [clearError, ipc, refreshSessions, reportError, retryingRunId],
  );

  const interruptible =
    projection.activeRunId !== null || activeSession?.status === "running";
  const runStatusText = projection.activeRunId
    ? "run 运行中"
    : (projection.runs.at(-1)?.status ?? "无进行中 run");

  return (
    <section className="workbench" data-testid="workbench">
      {/* T1 埋点锚点（M4-04 读取；`data-ms` 为空表示尚未创建会话）。 */}
      <span
        hidden
        data-testid="create-latency-ms"
        data-ms={createLatencyMs === null ? "" : String(createLatencyMs)}
      />
      <TopBar
        session={activeSession}
        runId={projection.activeRunId}
        runStatusText={runStatusText}
        interruptible={interruptible}
        onInterrupt={onInterrupt}
        lastAck={lastAck}
        runtimes={runtimes}
        pendingCount={pendingPermissionCount}
        rightOpen={rightOpen}
        onToggleRight={() => setRightOpen((open) => !open)}
      />
      <div className="workbench-columns">
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
                // M3-03 DoD3：disabled 运行时不可选（不可创建会话；D5）。
                disabled={runtime.status === "disabled"}
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
          {!activeSession ? (
            <p className="workbench-hint" data-testid="no-session-hint">
              请选择或新建会话
            </p>
          ) : null}

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
              renderItem={(bubble) => {
                // M3-06：失败/中断的 run 提供重试（仅终态 run 可重试；降级期禁用）。
                const runView = bubble.runId ? runViews.get(bubble.runId) : undefined;
                const retryable =
                  bubble.role === "assistant" &&
                  (runView?.status === "failed" || runView?.status === "cancelled");
                const interruptedByDegraded =
                  runView?.status === "cancelled" &&
                  runView.cancelReason === "persist_degraded";
                return (
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
                    {retryable && bubble.runId ? (
                      <div
                        className="message-run-actions"
                        data-testid="run-actions"
                        data-run-id={bubble.runId}
                      >
                        <span
                          className="run-outcome"
                          data-testid="run-outcome"
                          data-status={runView?.status}
                          data-code={runView?.errorCode ?? ""}
                        >
                          {runView?.status === "failed"
                            ? `运行失败：${runView.errorCode ?? "run_failed"}`
                            : interruptedByDegraded
                              ? "已中断（存储降级）"
                              : "已中断"}
                        </span>
                        <button
                          type="button"
                          data-testid="run-retry"
                          data-run-id={bubble.runId}
                          disabled={degraded || retryingRunId !== null}
                          onClick={() => void onRetryRun(bubble.runId as string)}
                        >
                          重试
                        </button>
                      </div>
                    ) : null}
                  </article>
                );
              }}
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

          {backpressureScope ? (
            <p
              className="storage-backpressure-notice"
              data-testid="storage-backpressure-notice"
              data-scope={backpressureScope}
              role="status"
            >
              {backpressureScope === "run"
                ? "存储写队列高水位（L2）：新任务暂被拒绝，队列回落至 ≤1024 后自动恢复。"
                : `适配器 ${isolatedRuntime?.id ?? ""} 背压隔离中（storage_backpressure）：重启后自动解除。`}
            </p>
          ) : null}

          <div className="composer" data-testid="composer">
            <textarea
              data-testid="composer-input"
              placeholder="输入消息（Ctrl+Enter 发送）"
              value={draft}
              disabled={!activeSessionId || degraded}
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
              disabled={!activeSessionId || degraded || sending || draft.trim().length === 0}
              onClick={() => void onSend()}
            >
              发送
            </button>
            {degraded ? (
              // M3-06：降级（只读）期发送入口禁用；恢复走 app_restart（D4 无热恢复）。
              <p
                className="composer-degraded-hint"
                data-testid="composer-degraded-hint"
                role="alert"
              >
                存储降级（只读）：发送已禁用；修复磁盘/目录/权限后重启应用。
              </p>
            ) : null}
          </div>

          {error ? (
            <p
              className="workbench-error"
              data-testid="workbench-error"
              data-code={errorCode ?? ""}
              role="alert"
            >
              {error}
            </p>
          ) : null}
        </div>

        {/* M3-03：右栏（S-04 运行时状态 + S-03 权限待办）；<1280px 由 CSS 折叠为抽屉，
            `right-panel-toggle` 控制 data-open（断点行为见 styles.css）。 */}
        <aside
          className="workbench-right"
          data-testid="right-panel"
          data-open={String(rightOpen)}
        >
          <RuntimePanel runtimes={runtimes} ipc={permissionIpc} onRefresh={refreshRuntimes} />
          <PermissionPanel
            ipc={permissionIpc}
            sessionId={activeSessionId}
            events={eventsState.events}
            onPendingCount={setPendingPermissionCount}
          />
        </aside>
      </div>
    </section>
  );
}
