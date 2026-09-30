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
import { artifactsIpc as productionArtifactsIpc, type ArtifactsIpc } from "./artifacts";
import { describeIpcError, ipcErrorCode } from "./startup";
import { diagnosticsIpc as productionDiagnosticsIpc, type DiagnosticsIpc } from "./diagnostics";
import { DiagnosticsEntry } from "./DiagnosticsEntry";
import type { EventStore } from "./eventStore";
import { FilePanel } from "./FilePanel";
import { STORAGE_L2_THRESHOLD } from "./health";
import { useStorageHealth } from "./healthBus";
import { HistoryOverflowNotice } from "./HistoryOverflowNotice";
import { Markdown } from "./markdown";
import { ModelSelector } from "./ModelSelector";
import { permissionIpc as productionPermissionIpc, type PermissionIpc } from "./permission";
import { PermissionPanel } from "./PermissionPanel";
import { providersIpc as productionProvidersIpc, type ProvidersIpc } from "./providers";
import { RuntimePanel } from "./RuntimePanel";
import {
  RUNTIME_STATUS_LABELS,
  SESSION_STATUS_LABELS,
  sessionIpc,
  supportsThinkingDepth,
  THINKING_DEPTH_DEFAULT,
  THINKING_DEPTH_LABELS,
  THINKING_DEPTH_MAX,
  THINKING_DEPTH_MIN,
  THINKING_DEPTH_UNSUPPORTED_CODE,
  THINKING_DEPTH_UNSUPPORTED_HINT,
  type MessageRow,
  type RuntimeInfo,
  type SessionIpc,
  type SessionSummary,
  type SessionWarning,
} from "./session";
import { projectSession } from "./sessionProjection";
import {
  groupSessions,
  SESSION_GROUP_TITLES,
} from "./sessionGroups";
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

/** 思考深度档位 tick 顺序（关闭/低/高/极高/最大；ADR-010）。 */
const THINKING_DEPTH_TICKS = [
  THINKING_DEPTH_MIN,
  1,
  THINKING_DEPTH_DEFAULT,
  3,
  THINKING_DEPTH_MAX,
];

/** 从响应 `warnings` 提取思考深度非阻断提示（无则 `null`）。 */
function thinkingWarningOf(warnings?: SessionWarning[]): string | null {
  const warning = warnings?.find(
    (item) => item.code === THINKING_DEPTH_UNSUPPORTED_CODE,
  );
  return warning?.message ?? null;
}

export interface SessionWorkbenchProps {
  store?: EventStore;
  ipc?: SessionIpc;
  permissionIpc?: PermissionIpc;
  historyLimit?: number;
  streamItemHeight?: number;
  streamHeight?: number;
  /** M3-05：右栏诊断分区（提供时渲染；打开 S-07 诊断导出页）。 */
  onOpenDiagnostics?: () => void;
  /** M3-05：诊断 IPC 注入（测试替身；缺省 = 生产 Tauri 实现）。 */
  diagnosticsIpc?: DiagnosticsIpc;
  /** M3-09：文件引用面板 IPC 注入（测试替身；缺省 = 生产 Tauri 实现）。 */
  artifactsIpc?: ArtifactsIpc;
  /** M3-11：供应商配置 IPC 注入（模型选择器派生数据源；缺省 = 生产 Tauri 实现）。 */
  providersIpc?: ProvidersIpc;
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
  onOpenDiagnostics,
  diagnosticsIpc: diagnosticsIpcProp = productionDiagnosticsIpc,
  artifactsIpc: artifactsIpcProp = productionArtifactsIpc,
  providersIpc: providersIpcProp = productionProvidersIpc,
}: SessionWorkbenchProps) {
  const [runtimes, setRuntimes] = useState<RuntimeInfo[]>([]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<MessageRow[]>([]);
  const [selectedRuntime, setSelectedRuntime] = useState<string>("");
  const [modelDraft, setModelDraft] = useState("");
  /** M3-11：输入区模型选择（作用于新建会话；与手动输入共用 `modelDraft` 草稿字段）。 */
  const [selectedModelId, setSelectedModelId] = useState<string | null>(null);
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
  /** M3-10：思考深度档位（0–4；随会话生效值回显）与弹层开关（原型输入区滑块）。 */
  const [thinkingDepth, setThinkingDepth] = useState<number>(THINKING_DEPTH_DEFAULT);
  const [thinkingOpen, setThinkingOpen] = useState(false);
  /** M3-10：同步能力门非阻断提示（`thinking_depth_unsupported`；无则 `null`）。 */
  const [thinkingWarning, setThinkingWarning] = useState<string | null>(null);
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
  /** M3-10：所选运行时能力预判（滑块置灰依据；`runtimes_list` 快照）。 */
  const selectedRuntimeInfo = useMemo(
    () => runtimes.find((runtime) => runtime.id === selectedRuntime) ?? null,
    [runtimes, selectedRuntime],
  );
  const thinkingSupported = supportsThinkingDepth(selectedRuntimeInfo);
  /** M3-06：run 视图索引（消息气泡 → 终态/取消原因 → 重试入口）。 */
  const runViews = useMemo(
    () => new Map(projection.runs.map((run) => [run.runId, run])),
    [projection.runs],
  );
  /**
   * M3-12/ADR-011：会话列表分组（固定组序 + 空组隐藏 + 组内 `updated_at` 倒序）。
   *
   * 直接计算（不 `useMemo`）：会话列表规模小；且对调用方原地更新列表数组（测试替身
   * 或增量维护）保持视图一致，不引入引用不变导致的陈旧分组。
   */
  const sessionGroups = groupSessions(sessions);

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

  // M3-10：会话切换/生效值变化时回显（延迟判定路径以 `SessionSummary.thinking_depth`
  // 生效值为准，ADR-010 v0.4；会话列表刷新但生效值不变时不重置用户当前调整）。
  const activeSessionDepth = activeSession?.thinking_depth ?? THINKING_DEPTH_DEFAULT;
  useEffect(() => {
    setThinkingDepth(activeSessionDepth);
    setThinkingWarning(null);
  }, [activeSessionId, activeSessionDepth]);

  /**
   * M3-11：模型选择器选择（作用于新建会话；M3-11 前置登记：与手动输入共存）。
   * 选择结果写入 `modelDraft`（UI-05 既有透传路径），不引入运行期切换入口。
   */
  const onSelectModel = useCallback((modelId: string | null) => {
    setSelectedModelId(modelId);
    setModelDraft(modelId ?? "");
  }, []);

  /** M3-11：手动输入模型（保留 UI-05 路径）；与选择器选择不一致时清空选择态。 */
  const onModelDraftChange = useCallback((value: string) => {
    setModelDraft(value);
    setSelectedModelId((current) => (current !== null && current !== value ? null : current));
  }, []);

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
        // M3-10/ADR-010：会话级思考深度随 `session_create` 透传（缺省 2）。
        thinking_depth: thinkingDepth,
      });
      // M3-10：同步判定路径非阻断警告（delta 等；延迟判定路径无警告，以回显为准）。
      setThinkingWarning(thinkingWarningOf(created.warnings));
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
  }, [
    ipc,
    modelDraft,
    refreshSessions,
    reportError,
    runtimes,
    selectedRuntime,
    thinkingDepth,
    titleDraft,
  ]);

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
        // M3-10/ADR-010：本次 run 覆盖（仅本次 run，不回写会话级值）。
        thinking_depth: thinkingDepth,
      });
      setThinkingWarning(thinkingWarningOf(ack.warnings));
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
  }, [
    activeSessionId,
    clearError,
    draft,
    ipc,
    refreshSessions,
    reportError,
    thinkingDepth,
  ]);

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
            {sessionGroups.map(({ group, sessions: groupedSessions }) => (
              <li
                key={group}
                className={`session-group session-group-${group}`}
                data-testid="session-group"
                data-group={group}
              >
                <span className="session-group-title" data-testid="session-group-title">
                  {SESSION_GROUP_TITLES[group]}
                </span>
                <ul className="session-group-items">
                  {groupedSessions.map((session) => (
                    <li key={session.id}>
                      <button
                        type="button"
                        data-testid="session-item"
                        data-session-id={session.id}
                        data-active={String(session.id === activeSessionId)}
                        data-status={session.status}
                        className={
                          session.id === activeSessionId ? "session-item active" : "session-item"
                        }
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
                </ul>
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
              onChange={(event) => onModelDraftChange(event.target.value)}
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
            {/* M3-10/ADR-010：思考深度 5 档滑块（关闭/低/高/极高/最大，默认高）；
                运行时未声明 `thinking_depth` 能力时滑块置灰 + tooltip。 */}
            <div className="thinking-control">
              <button
                type="button"
                className="thinking-toggle"
                data-testid="thinking-toggle"
                data-value={thinkingDepth}
                data-enabled={thinkingSupported}
                title={thinkingSupported ? "思考深度" : THINKING_DEPTH_UNSUPPORTED_HINT}
                aria-label="思考深度"
                aria-expanded={thinkingOpen}
                onClick={() => setThinkingOpen((open) => !open)}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <path d="M9.5 2A2.5 2.5 0 0112 4.5v15a2.5 2.5 0 01-4.96.44 2.5 2.5 0 01-2.96-3.08 3 3 0 01-.34-5.58 2.5 2.5 0 011.32-4.24 2.5 2.5 0 011.98-3A2.5 2.5 0 019.5 2z" />
                  <path d="M14.5 2A2.5 2.5 0 0012 4.5v15a2.5 2.5 0 004.96.44 2.5 2.5 0 002.96-3.08 3 3 0 00.34-5.58 2.5 2.5 0 00-1.32-4.24 2.5 2.5 0 00-1.98-3A2.5 2.5 0 0014.5 2z" />
                </svg>
                <span data-testid="thinking-current">
                  {THINKING_DEPTH_LABELS[thinkingDepth]}
                </span>
              </button>
              {!thinkingSupported ? (
                <p
                  className="thinking-disabled-hint"
                  data-testid="thinking-disabled-hint"
                  data-enabled="false"
                  role="note"
                >
                  {THINKING_DEPTH_UNSUPPORTED_HINT}
                </p>
              ) : null}
              {thinkingOpen ? (
                <div
                  className="thinking-popover"
                  data-testid="thinking-popover"
                  data-enabled={thinkingSupported}
                >
                  <div className="thinking-head">
                    <span>思考深度</span>
                    <span className="thinking-value">
                      {THINKING_DEPTH_LABELS[thinkingDepth]}
                    </span>
                  </div>
                  <input
                    type="range"
                    min={THINKING_DEPTH_MIN}
                    max={THINKING_DEPTH_MAX}
                    step={1}
                    value={thinkingDepth}
                    data-testid="thinking-slider"
                    data-value={thinkingDepth}
                    data-enabled={thinkingSupported}
                    disabled={!thinkingSupported}
                    aria-label="思考深度"
                    onChange={(event) => setThinkingDepth(Number(event.target.value))}
                  />
                  <div className="thinking-ticks">
                    {THINKING_DEPTH_TICKS.map((value) => (
                      <button
                        key={value}
                        type="button"
                        data-value={value}
                        data-active={value === thinkingDepth}
                        disabled={!thinkingSupported}
                        onClick={() => setThinkingDepth(value)}
                      >
                        {THINKING_DEPTH_LABELS[value]}
                      </button>
                    ))}
                  </div>
                </div>
              ) : null}
            </div>
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
            <div className="composer-toolbar">
              {/* M3-11：输入区模型选择器（仅启用供应商的启用模型；作用于新建会话）。 */}
              <ModelSelector
                ipc={providersIpcProp}
                value={selectedModelId}
                onChange={onSelectModel}
                sessionModel={activeSession?.model ?? null}
                disabled={degraded}
              />
              <button
                type="button"
                data-testid="composer-send"
                disabled={!activeSessionId || degraded || sending || draft.trim().length === 0}
                onClick={() => void onSend()}
              >
                发送
              </button>
            </div>
            {thinkingWarning ? (
              <p
                className="thinking-warning"
                data-testid="thinking-warning"
                role="status"
              >
                {thinkingWarning}
              </p>
            ) : null}
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
          {/* M3-09 S-11：文件引用面板（只读；会话文件/项目文件；ADR-010 决策 1）。 */}
          <FilePanel
            ipc={artifactsIpcProp}
            sessionId={activeSessionId}
            workspaceRoot={activeSession?.workspace_root ?? null}
          />
          {onOpenDiagnostics ? (
            <DiagnosticsEntry ipc={diagnosticsIpcProp} onOpen={onOpenDiagnostics} />
          ) : null}
        </aside>
      </div>
    </section>
  );
}
