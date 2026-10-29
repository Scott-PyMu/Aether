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
 * `@aether/protocol` typed client）与 `permissionIpc`（权限/运行时控制命令面，默认
 * typed client）——E2E/单测注入替身即可覆盖完整交互路径。
 */
import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent, type ReactNode } from "react";

import { appEventStore } from "./aetherStore";
import { archiveIpc as productionArchiveIpc, type ArchiveIpc } from "./archive";
import { artifactsIpc as productionArtifactsIpc, type ArtifactsIpc } from "./artifacts";
import { describeIpcError, ipcErrorCode } from "./startup";
import {
  defaultAetherEventListener,
  type AetherEventListener,
} from "./eventBridge";
import type { EventStore } from "./eventStore";
import { FilePanel } from "./FilePanel";
import { STORAGE_L2_THRESHOLD } from "./health";
import { useStorageHealth } from "./healthBus";
import { HistoryOverflowNotice } from "./HistoryOverflowNotice";
import {
  activateTab,
  clampLayout,
  closeTab,
  defaultLayout,
  loadLayout,
  openTab,
  saveLayout,
  setSplit,
  type WorkbenchLayout,
} from "./layout";
import { Markdown } from "./markdown";
import { ModelSelector } from "./ModelSelector";
import { permissionIpc as productionPermissionIpc, type PermissionIpc } from "./permission";
import { PermissionModeSelector } from "./PermissionModeSelector";
import { PermissionPanel } from "./PermissionPanel";
import { presetsIpc as productionPresetsIpc, type PresetEntry, type PresetsIpc } from "./presets";
import {
  firstEnabledModel,
  parseProviderSelectionMemory,
  providerSupportsRuntime,
  providersForRuntime,
  providersIpc as productionProvidersIpc,
  rememberedProvider,
  requiresProviderSelection,
  type ProviderEntry,
  type ProviderSelectionMemory,
  type ProvidersIpc,
} from "./providers";
import { RuntimePanel } from "./RuntimePanel";
import {
  normalizePermissionMode,
  PAUSED_STATUS_HINT,
  PAUSE_SEMANTICS_NOTICE,
  RUNTIME_STATUS_LABELS,
  SESSION_STATUS_LABELS,
  sessionIpc,
  supportsPauseResume,
  supportsThinkingDepth,
  THINKING_DEPTH_DEFAULT,
  THINKING_DEPTH_LABELS,
  THINKING_DEPTH_MAX,
  THINKING_DEPTH_MIN,
  THINKING_DEPTH_UNSUPPORTED_CODE,
  THINKING_DEPTH_UNSUPPORTED_HINT,
  type MessageRow,
  type PermissionMode,
  type RunQueuePosition,
  type RuntimeInfo,
  type SessionIpc,
  type SessionReference,
  type SessionSearchHit,
  type SessionSummary,
  type SessionWarning,
} from "./session";
import { projectSession } from "./sessionProjection";
import {
  groupSessions,
  SESSION_GROUP_TITLES,
} from "./sessionGroups";
import {
  settingsIpc as productionSettingsIpc,
  UI_PROVIDER_SELECTION_KEY,
  type SettingsIpc,
} from "./settings";
import { SplitPane } from "./SplitPane";
import { TopBar } from "./TopBar";
import { generateUlid } from "./ulid";
import { useSessionEvents } from "./useSessionEvents";
import { VirtualList } from "./VirtualList";
import { WorkbenchTabs } from "./WorkbenchTabs";
import { workspacesIpc as productionWorkspacesIpc, type WorkspacesIpc } from "./workspace";
import { WorkspaceTree } from "./WorkspaceTree";

/** 历史消息加载条数（`messages_page` 最近一页；与 EventStore 重载默认一致）。 */
export const DEFAULT_HISTORY_LIMIT = 500;
/** 虚拟列表行高 / 视口（像素；消息气泡按内容高度渲染在行内）。 */
export const STREAM_ITEM_HEIGHT = 96;
export const STREAM_VIEWPORT_HEIGHT = 480;
/** 空会话占位 id（未选会话时 hook 仍需一个稳定键）。 */
const NO_SESSION = "__none__";

/** 思考深度档位 tick 顺序（关闭/低/高/极高/最高；ADR-010）。 */
const THINKING_DEPTH_TICKS = [
  THINKING_DEPTH_MIN,
  1,
  THINKING_DEPTH_DEFAULT,
  3,
  THINKING_DEPTH_MAX,
];

/** 运行时选择器配色（与原型 agent picker 一致）。 */
const RUNTIME_ACCENTS: Record<string, string> = {
  claude: "#D97706",
  codex: "#16A34A",
  dsh: "#2F6FD0",
  deepseek: "#4D6BFE",
};

function runtimeAccentOf(runtime: RuntimeInfo | null | undefined): string {
  if (!runtime) {
    return "#666666";
  }
  return RUNTIME_ACCENTS[runtime.kind] ?? RUNTIME_ACCENTS[runtime.id] ?? "#666666";
}

function runtimeAbbrOf(runtime: RuntimeInfo | null | undefined): string {
  if (!runtime) {
    return "?";
  }
  if (runtime.kind in RUNTIME_ACCENTS) {
    return runtime.kind === "claude" ? "CC" : runtime.kind === "codex" ? "CX" : "DS";
  }
  const letters = runtime.name.replace(/[^A-Za-z]/g, "");
  return (letters.slice(0, 2) || runtime.name.slice(0, 2)).toUpperCase();
}

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
  /** M3-05：打开 S-07 诊断导出页（用户菜单入口）。 */
  onOpenDiagnostics?: () => void;
  /** M3-09：文件引用面板 IPC 注入（测试替身；缺省 = 生产 Tauri 实现）。 */
  artifactsIpc?: ArtifactsIpc;
  /** M3-11：供应商配置 IPC 注入（模型选择器派生数据源；缺省 = 生产 Tauri 实现）。 */
  providersIpc?: ProvidersIpc;
  /** M5-05：归档导出 IPC 注入（单会话导出；缺省 = 生产 Tauri 实现）。 */
  archiveIpc?: ArchiveIpc;
  /**
   * M7-01/ADR-015（DoD0）：`aether://event` 监听注入（测试替身；缺省 = 生产
   * tauri-specta 生成绑定）。`runtime.status_changed` 到达时实时刷新运行时注册表
   * （徽标即时变化；health 轮询保留为兜底）。
   */
  eventListener?: AetherEventListener;
  /**
   * M7-04/ADR-018 附录 A-6：布局持久化 IPC（`ui.layout`；缺省 = 生产实现；
   * 测试注入替身以覆盖重启恢复与损坏回退）。
   */
  settingsIpc?: SettingsIpc;
  /**
   * M7-09/ADR-026：Agent 预设 IPC（新建会话入口的预设选择数据源；
   * 缺省 = 生产 Tauri 实现；测试注入替身）。
   */
  presetsIpc?: PresetsIpc;
  /** M7-09/ADR-026：新建会话入口当前选中的预设（由 `App` 持有，供设置页「使用中」标记）。 */
  activePresetId?: string | null;
  /** M7-09/ADR-026：预设选择变更（缺省 = 本地只读展示）。 */
  onActivePresetChange?: (presetId: string | null) => void;
  /** M7-09/ADR-026：预设清单版本（设置页增改删后递增 → 重新拉取清单）。 */
  presetsVersion?: number;
  /**
   * M7-08/ADR-026 §2.1：左栏工作区树 IPC（工作区管理五命令；缺省 = 生产实现；
   * 测试注入替身以覆盖创建/改名/删除/改派与错误路径）。
   */
  workspacesIpc?: WorkspacesIpc;
  /**
   * UI 版式（对齐 P0 原型）：侧栏用户菜单入口与版本信息。
   * 均为可选——嵌入/测试场景缺省不渲染入口，不影响既有锚点。
   */
  onOpenSettings?: () => void;
  onOpenBackup?: () => void;
  onOpenAudit?: () => void;
  onOpenAbout?: () => void;
  appVersion?: string;
  protocolVersion?: { major: number; minor: number };
  dataDir?: string;
  /** 侧栏底部状态槽（健康横幅 / 事件通道指示；由 App 注入）。 */
  statusSlot?: ReactNode;
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

/** 单次发送引用条数上限（M7-06/ADR-024 §2.1-5：≤32 条；核心命令层同值校验）。 */
export const MAX_SESSION_REFERENCES = 32;

/**
 * 拖拽路径 → 引用类型（拖拽事件只携带路径，无类型信息）。
 *
 * 启发式：基名含 `.`（且非尾点）视为文件，否则视为目录；仅影响引用块呈现文本，
 * 路径可访问性与 canonicalize 由核心命令层裁决（`artifact_path_rejected`）。
 */
export function kindForPath(path: string): SessionReference["kind"] {
  const base = path.split(/[\\/]/).pop() ?? "";
  if (base.length === 0) {
    return "file";
  }
  return base.includes(".") && !base.endsWith(".") ? "file" : "directory";
}

export function SessionWorkbench({
  store = appEventStore,
  ipc = sessionIpc,
  permissionIpc = productionPermissionIpc,
  historyLimit = DEFAULT_HISTORY_LIMIT,
  streamItemHeight = STREAM_ITEM_HEIGHT,
  streamHeight = STREAM_VIEWPORT_HEIGHT,
  onOpenDiagnostics,
  artifactsIpc: artifactsIpcProp = productionArtifactsIpc,
  providersIpc: providersIpcProp = productionProvidersIpc,
  archiveIpc: archiveIpcProp = productionArchiveIpc,
  eventListener = defaultAetherEventListener,
  settingsIpc: settingsIpcProp = productionSettingsIpc,
  presetsIpc: presetsIpcProp = productionPresetsIpc,
  activePresetId = null,
  onActivePresetChange,
  presetsVersion = 0,
  workspacesIpc: workspacesIpcProp = productionWorkspacesIpc,
  onOpenSettings,
  onOpenBackup,
  onOpenAudit,
  onOpenAbout,
  appVersion,
  protocolVersion,
  dataDir,
  statusSlot,
}: SessionWorkbenchProps) {
  const [runtimes, setRuntimes] = useState<RuntimeInfo[]>([]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  /**
   * M7-07/ADR-025：会话检索（Ctrl+K 聚焦；标题 + 正文 FTS）。
   * `searchHits === null` = 未检索（显示常规分组列表）；空数组 = 无命中。
   */
  const [searchQuery, setSearchQuery] = useState("");
  const [searchHits, setSearchHits] = useState<SessionSearchHit[] | null>(null);
  const [searchBusy, setSearchBusy] = useState(false);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  /** M7-07：改名/删除的进行中与错误态（锚点 `session-rename*` / `session-delete*`）。 */
  const [renamingSessionId, setRenamingSessionId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [renameBusy, setRenameBusy] = useState(false);
  const [deletingSessionId, setDeletingSessionId] = useState<string | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [manageError, setManageError] = useState<{ code: string | null; message: string } | null>(
    null,
  );
  /**
   * M7-04（UI-01 完整）：工作台多视图布局（标签页/分屏/聚焦窗格）。
   *
   * - 持久化落点 = settings `ui.layout`（ADR-018 附录 A-6）；损坏回退默认布局；
   * - 首次运行（无持久化布局）延续 P0 行为：自动打开首个会话；
   * - 用户显式操作（打开/切换/关闭/分屏）后立即持久化。
   */
  const [layout, setLayout] = useState<WorkbenchLayout>(defaultLayout);
  /** 布局是否为「显式」（持久化存在或用户已操作）——决定是否自动打开首个会话。 */
  const layoutExplicit = useRef(false);
  const activeSessionId = layout.active;
  const [messages, setMessages] = useState<MessageRow[]>([]);
  const [selectedRuntime, setSelectedRuntime] = useState<string>("");
  /**
   * M7-02/ADR-022：自动路由开关（缺省关闭 = 手动指定优先；开启后 `session_create`
   * 不带 `runtime_id`，由固定优先级常量路由；失败回 `no_candidate_runtime`）。
   */
  const [autoRoute, setAutoRoute] = useState(false);
  const [modelDraft, setModelDraft] = useState("");
  /** M3-11：输入区模型选择（作用于新建会话；与手动输入共用 `modelDraft` 草稿字段）。 */
  const [selectedModelId, setSelectedModelId] = useState<string | null>(null);
  /** ADR-028：选择器选中的模型所属供应商（新建会话透传 `provider_id`）。 */
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  /** ADR-028 v0.9：供应商目录（运行时过滤/记忆预填/旧会话补选共用）。 */
  const [providerCatalog, setProviderCatalog] = useState<ProviderEntry[]>([]);
  /** ADR-028 v0.9：全局供应商记忆（`ui.provider_selection`；runtime → provider_id）。 */
  const [providerMemory, setProviderMemory] = useState<ProviderSelectionMemory>({});
  /** 目录/记忆是否已加载（预填仅在加载完成后执行一次，不覆盖用户后续选择）。 */
  const [providerCatalogLoaded, setProviderCatalogLoaded] = useState(false);
  /** ADR-028 v0.9：旧会话补选草稿（仅 provider 为空时允许 provider 切换）。 */
  const [reselectProviderId, setReselectProviderId] = useState<string | null>(null);
  const [reselectModelId, setReselectModelId] = useState<string | null>(null);
  const [titleDraft, setTitleDraft] = useState("");
  /**
   * M7-09/ADR-026：Agent 预设清单（新建会话入口可选；读取失败静默降级为空清单——
   * 预设为可选能力，不阻断工作台）。
   */
  const [presets, setPresets] = useState<PresetEntry[]>([]);
  const [draft, setDraft] = useState("");
  /**
   * M7-06/ADR-024：随消息附带的文件引用（待发送；`@` 入口 + 拖拽添加，≤32 条；
   * 发送成功后清空；**引用 ≠ 预授权**——Agent 读取仍走 `fs.read` 权限门）。
   */
  const [pendingReferences, setPendingReferences] = useState<SessionReference[]>([]);
  const composerRef = useRef<HTMLDivElement | null>(null);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 结构化错误码（UI-UX §7.3 通用行：错误元素补 `data-code`；无码为 `null`）。 */
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [lastAck, setLastAck] = useState<{
    runId: string;
    duplicate: boolean;
    /** M7-02/ADR-022：ack 所属会话（闸门排队位置展示的作用域判定）。 */
    sessionId: string;
    /** M7-02/ADR-022：超闸门排队位置（无排队为 `null`）。 */
    queue: RunQueuePosition | null;
  } | null>(null);
  /** T1 埋点：会话创建（点击 → `session_create` ack）耗时（毫秒）。 */
  const [createLatencyMs, setCreateLatencyMs] = useState<number | null>(null);
  /** M3-06：重放进行中的 run（按钮去抖）。 */
  const [retryingRunId, setRetryingRunId] = useState<string | null>(null);
  /** M3-03：右栏（运行时/权限）抽屉开关（<1280px 断点；≥1280 常驻）。 */
  const [rightOpen, setRightOpen] = useState(true);
  /** 原型版式：Agent / Chat 双模式（Chat 为 P2 占位，仅切换展示）。 */
  const [mode, setMode] = useState<"agent" | "chat">("agent");
  /** 原型版式：左栏折叠 / 新建会话展开 / 用户菜单。 */
  const [sideCollapsed, setSideCollapsed] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [userMenuOpen, setUserMenuOpen] = useState(false);
  /** 右栏分区：文件（原型默认）/ 改动（P1 占位）。 */
  const [rightTab, setRightTab] = useState<"files" | "changes">("files");
  /** 输入区 Agent 运行时选择器（原型 agent picker）。 */
  const [agentPickerOpen, setAgentPickerOpen] = useState(false);
  /** M3-03：待审批计数（全局面板数据源 → 顶栏徽标）。 */
  const [pendingPermissionCount, setPendingPermissionCount] = useState(0);
  /** M3-10：思考深度档位（0–4；随会话生效值回显）与弹层开关（原型输入区滑块）。 */
  const [thinkingDepth, setThinkingDepth] = useState<number>(THINKING_DEPTH_DEFAULT);
  const [thinkingOpen, setThinkingOpen] = useState(false);
  /** M3-10：同步能力门非阻断提示（`thinking_depth_unsupported`；无则 `null`）。 */
  const [thinkingWarning, setThinkingWarning] = useState<string | null>(null);
  /** M6-01/ADR-014 决策 3：会话级权限模式（缺省 `workspace_write`；随会话回显）。 */
  const [permissionMode, setPermissionMode] = useState<PermissionMode>("workspace_write");
  /** M7-03/ADR-017：暂停/恢复进行中（顶栏按钮去抖）。 */
  const [pauseBusy, setPauseBusy] = useState(false);
  /** M5-05：单会话导出进行中（顶栏按钮去抖）与结果回执（`session-export-result`）。 */
  const [exportBusy, setExportBusy] = useState(false);
  const [exportResult, setExportResult] = useState<{
    result: "ok" | "failed";
    message: string;
  } | null>(null);
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
  /** ADR-028 v0.9：活动会话的供应商/模型状态（只读、补选与优先级）。 */
  const activeRequiresProvider =
    activeSession?.provider_state !== undefined &&
    requiresProviderSelection(activeSession.runtime_id);
  const activeProviderState = activeSession?.provider_state ?? "missing";
  const activeModelState = activeSession?.model_state ?? "missing";
  const activeProvider = activeSession?.provider_id
    ? (providerCatalog.find((provider) => provider.id === activeSession.provider_id) ?? null)
    : null;
  const activeProviderModels = activeProvider?.models.filter((model) => model.enabled) ?? [];
  const reselectOptions = useMemo(
    () =>
      activeSession
        ? providersForRuntime(providerCatalog, activeSession.runtime_id)
        : [],
    [activeSession, providerCatalog],
  );
  const selectionRunInFlight =
    activeSession?.status === "running" || activeSession?.status === "waiting_permission";
  // M7-04：布局恢复（settings `ui.layout`；损坏回退默认布局，ADR-018 附录 A-6）。
  // 仅在挂载时读取一次；后续由显式操作写回（失败静默，不阻断工作台）。
  // 注意：无持久化布局（首次运行）时**不覆盖**已发生的 P0 自动选中；读取与
  // `session_list` 的完成顺序不可假设。
  useEffect(() => {
    let active = true;
    void loadLayout(settingsIpcProp).then((result) => {
      if (!active) {
        return;
      }
      if (layoutExplicit.current) {
        // 用户在布局读取完成前已显式操作：以用户操作为准，不覆盖。
        return;
      }
      const restored = result.persisted || result.corrupted;
      layoutExplicit.current = restored;
      if (restored) {
        setLayout(result.layout);
      }
    });
    return () => {
      active = false;
    };
  }, [settingsIpcProp]);
  // M7-09/ADR-026：预设清单（新建会话入口可选；失败静默降级——不阻断会话创建）。
  useEffect(() => {
    let active = true;
    void presetsIpcProp
      .listPresets()
      .then((list) => {
        if (active) {
          setPresets(list);
        }
      })
      .catch(() => {
        if (active) {
          setPresets([]);
        }
      });
    return () => {
      active = false;
    };
  }, [presetsIpcProp, presetsVersion]);
  // ADR-028 v0.9：供应商目录 + 全局供应商记忆（读取失败静默降级：不自动选择）。
  useEffect(() => {
    let active = true;
    void Promise.all([
      providersIpcProp.list().catch(() => [] as ProviderEntry[]),
      settingsIpcProp
        .get(UI_PROVIDER_SELECTION_KEY)
        .then((result) => parseProviderSelectionMemory(result.value))
        .catch(() => ({}) as ProviderSelectionMemory),
    ]).then(([providers, memory]) => {
      if (!active) return;
      setProviderCatalog(providers);
      setProviderMemory(memory);
      setProviderCatalogLoaded(true);
    });
    return () => {
      active = false;
    };
  }, [providersIpcProp, settingsIpcProp]);
  /** M7-04：打开的标签会话（按标签顺序；过滤已不存在的会话）。 */
  const tabSessions = useMemo(() => {
    const byId = new Map(sessions.map((session) => [session.id, session]));
    return layout.tabs
      .map((id) => byId.get(id))
      .filter((session): session is SessionSummary => session !== undefined);
  }, [layout.tabs, sessions]);
  /** M7-04：分屏第二窗格会话（`null` = 单窗格）。 */
  const splitSession = useMemo(
    () => sessions.find((session) => session.id === layout.split) ?? null,
    [layout.split, sessions],
  );
  /** M7-04：分屏开关可用（已开 → 可关；未开 → 需存在可放入第二窗格的其他标签）。 */
  const splitEnabled =
    layout.split !== null ||
    (layout.active !== null && layout.tabs.some((id) => id !== layout.active));
  /** M3-10：所选运行时能力预判（滑块置灰依据；`runtimes_list` 快照）。 */
  const selectedRuntimeInfo = useMemo(
    () => runtimes.find((runtime) => runtime.id === selectedRuntime) ?? null,
    [runtimes, selectedRuntime],
  );
  /** ADR-028 v0.9：新建会话的供应商选择与必选提示（静态能力，不依赖 hello/ready）。 */
  const createRequiresProvider = autoRoute || requiresProviderSelection(selectedRuntime);
  const availableCreateProviders = useMemo(
    () => providersForRuntime(providerCatalog, autoRoute ? null : selectedRuntime),
    [autoRoute, providerCatalog, selectedRuntime],
  );
  const createProviderMissing = createRequiresProvider && !selectedProviderId;
  const createModelMissing = createRequiresProvider && modelDraft.trim().length === 0;
  const thinkingSupported = supportsThinkingDepth(selectedRuntimeInfo);
  const thinkingPercent =
    (thinkingDepth / THINKING_DEPTH_MAX) * 100;
  /** M7-03/ADR-017：当前会话运行时的 `pause_resume` 能力（置灰依据；ADR-017 附录 A-5）。 */
  const activeSessionRuntime = useMemo(
    () => runtimes.find((runtime) => runtime.id === activeSession?.runtime_id) ?? null,
    [runtimes, activeSession],
  );
  const pauseSupported = supportsPauseResume(activeSessionRuntime);
  /** 暂停/恢复入口置灰：无会话 / 能力未声明 / 降级 / 状态不可暂停（creating/终态）。 */
  const pauseDisabled =
    !activeSession ||
    !pauseSupported ||
    degraded ||
    (activeSession.status !== "paused" &&
      activeSession.status !== "idle" &&
      activeSession.status !== "running" &&
      activeSession.status !== "waiting_permission");
  /** M3-06：run 视图索引（消息气泡 → 终态/取消原因 → 重试入口）。 */
  const runViews = useMemo(
    () => new Map(projection.runs.map((run) => [run.runId, run])),
    [projection.runs],
  );
  /**
   * M7-02/ADR-022：闸门排队位置（最近一次发送 ack 的 `queue` 快照；`run.started`
   * 到达（run 视图出现）即隐藏——仅当前会话、仅未派发窗口展示）。
   */
  const queueNotice = useMemo(() => {
    if (!lastAck?.queue || lastAck.sessionId !== activeSessionId) {
      return null;
    }
    return runViews.has(lastAck.runId) ? null : lastAck.queue;
  }, [lastAck, activeSessionId, runViews]);
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
      setLayout((current) => {
        // M7-04：会话删除/列表刷新 → 清掉悬空标签（active/split 随之归位）。
        const clamped = clampLayout(
          current,
          list.map((session) => session.id),
        );
        // 首次运行（无持久化布局且用户未操作）：延续 P0 行为自动打开首个会话。
        if (!layoutExplicit.current && clamped.tabs.length === 0) {
          const first = list[0];
          return first ? openTab(clamped, first.id) : clamped;
        }
        return clamped;
      });
    } catch (failure) {
      reportError(failure);
    }
  }, [ipc, reportError]);

  // M7-07/ADR-025 §2.1-2：搜索防抖（250ms）；空查询回到常规分组列表。
  useEffect(() => {
    const query = searchQuery.trim();
    if (!query) {
      setSearchHits(null);
      setSearchBusy(false);
      return;
    }
    let cancelled = false;
    setSearchBusy(true);
    const timer = window.setTimeout(() => {
      void ipc
        .searchSessions({ query, limit: 20 })
        .then((hits) => {
          if (!cancelled) {
            setSearchHits(hits);
            setManageError(null);
          }
        })
        .catch((failure: unknown) => {
          if (!cancelled) {
            setSearchHits([]);
            setManageError({
              code: ipcErrorCode(failure),
              message: describeIpcError(failure),
            });
          }
        })
        .finally(() => {
          if (!cancelled) {
            setSearchBusy(false);
          }
        });
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [ipc, searchQuery]);

  // M7-07（UI-UX §9）：Ctrl+K / Cmd+K 聚焦会话搜索。
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        searchInputRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const refreshActiveSearch = useCallback(async () => {
    const query = searchQuery.trim();
    if (!query) return;
    try {
      const hits = await ipc.searchSessions({ query, limit: 20 });
      setSearchHits(hits);
    } catch (failure) {
      setManageError({
        code: ipcErrorCode(failure),
        message: describeIpcError(failure),
      });
    }
  }, [ipc, searchQuery]);

  const submitRename = useCallback(async () => {
    if (!renamingSessionId || renameBusy) return;
    const title = renameDraft.trim();
    if (!title) {
      setManageError({ code: "invalid_value", message: "标题不能为空或纯空白" });
      return;
    }
    setRenameBusy(true);
    try {
      await ipc.renameSession(renamingSessionId, title);
      setRenamingSessionId(null);
      setManageError(null);
      await refreshSessions();
      await refreshActiveSearch();
    } catch (failure) {
      setManageError({
        code: ipcErrorCode(failure),
        message: describeIpcError(failure),
      });
    } finally {
      setRenameBusy(false);
    }
  }, [ipc, refreshActiveSearch, refreshSessions, renameBusy, renameDraft, renamingSessionId]);

  const confirmDelete = useCallback(async () => {
    if (!deletingSessionId || deleteBusy) return;
    setDeleteBusy(true);
    try {
      await ipc.deleteSession(deletingSessionId);
      setDeletingSessionId(null);
      setManageError(null);
      await refreshSessions();
      await refreshActiveSearch();
    } catch (failure) {
      setManageError({
        code: ipcErrorCode(failure),
        message: describeIpcError(failure),
      });
    } finally {
      setDeleteBusy(false);
    }
  }, [deleteBusy, deletingSessionId, ipc, refreshActiveSearch, refreshSessions]);

  /**
   * M7-04：显式布局操作（打开/切换/关闭/分屏）→ 状态更新 + 立即持久化
   * （`ui.layout`；写入失败静默——布局为非关键状态，下次操作重试）。
   */
  const applyLayout = useCallback(
    (next: WorkbenchLayout) => {
      if (next === layout) {
        return;
      }
      layoutExplicit.current = true;
      setLayout(next);
      void saveLayout(settingsIpcProp, next);
    },
    [layout, settingsIpcProp],
  );

  /** M7-04：打开/切换标签（左栏会话项点击与新建会话复用；不存在则追加标签）。 */
  const openSessionView = useCallback(
    (sessionId: string) => {
      applyLayout(openTab(layout, sessionId));
    },
    [applyLayout, layout],
  );

  /** M7-04：标签点击（聚焦；分屏窗格会话被聚焦时两窗格对调）。 */
  const onActivateTab = useCallback(
    (sessionId: string) => {
      applyLayout(activateTab(layout, sessionId));
    },
    [applyLayout, layout],
  );

  /** M7-04：关闭标签（不删除会话；聚焦/分屏归位）。 */
  const onCloseTab = useCallback(
    (sessionId: string) => {
      applyLayout(closeTab(layout, sessionId));
    },
    [applyLayout, layout],
  );

  /** M7-04：分屏开关（打开 = 第二个已打开标签进入右窗格；关闭 = 回单窗格）。 */
  const onToggleSplit = useCallback(() => {
    if (layout.split !== null) {
      applyLayout(setSplit(layout, null));
      return;
    }
    const other = layout.tabs.find((id) => id !== layout.active);
    if (!other) {
      return;
    }
    applyLayout(setSplit(layout, other));
  }, [applyLayout, layout]);

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

  // M7-01/ADR-015（DoD0）：`runtime.status_changed` 事件实时刷新运行时注册表
  // （合成归段 `runtime:<id>`；徽标/选择器即时变化）。监听注册失败不阻断工作台
  // （health 轮询仍为兜底刷新路径）。
  // M7-04（DoD4）：`session.status_changed` 到达时刷新会话列表——标签页后台状态
  // 角标（运行中/等待审批/失败）与 M3-12 分组口径同源（`sessionGroupOf`）。
  const refreshRuntimesRef = useRef(refreshRuntimes);
  refreshRuntimesRef.current = refreshRuntimes;
  const refreshSessionsRef = useRef(refreshSessions);
  refreshSessionsRef.current = refreshSessions;
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | null = null;
    void eventListener((event) => {
      if (event.type === "runtime.status_changed") {
        void refreshRuntimesRef.current();
      } else if (event.type === "session.status_changed") {
        void refreshSessionsRef.current();
      }
    })
      .then((dispose) => {
        if (active) {
          unlisten = dispose;
        } else {
          dispose();
        }
      })
      .catch(() => {
        // 事件通道不可用（如 jsdom 测试环境）：保持静默，由 health 轮询兜底。
      });
    return () => {
      active = false;
      if (unlisten) {
        unlisten();
      }
    };
  }, [eventListener]);

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

  // M6-01/ADR-014：会话切换/生效值变化时回显档位；非法值回退缺省 `workspace_write`
  // （会话列表刷新但档位不变时不重置用户当前调整，与思考深度回显同口径）。
  const activeSessionPermissionMode = normalizePermissionMode(
    activeSession?.permission_mode ?? null,
  );
  useEffect(() => {
    setPermissionMode(activeSessionPermissionMode);
  }, [activeSessionId, activeSessionPermissionMode]);

  /** M6-01/ADR-014：档位切换（`session_permission_mode_set`；失败走既有错误呈现）。 */
  const onChangePermissionMode = useCallback(
    async (mode: PermissionMode) => {
      if (!activeSessionId) {
        return;
      }
      clearError();
      try {
        await ipc.setPermissionMode(activeSessionId, mode);
        setPermissionMode(mode);
        await refreshSessions();
      } catch (failure) {
        reportError(failure);
      }
    },
    [activeSessionId, clearError, ipc, refreshSessions, reportError],
  );

  /**
   * M3-11：模型选择器选择（作用于新建会话；M3-11 前置登记：与手动输入共存）。
   * 选择结果写入 `modelDraft`（UI-05 既有透传路径），不引入运行期切换入口。
   */
  const onSelectModel = useCallback((modelId: string | null) => {
    setSelectedModelId(modelId);
    setModelDraft(modelId ?? "");
  }, []);

  /** ADR-028：模型选择器同时回传所属供应商（`null` = 清空）。 */
  const onModelProviderChange = useCallback((providerId: string | null) => {
    setSelectedProviderId(providerId);
  }, []);

  /** ADR-028 v0.9：显式选择供应商（新建会话）；默认带出该供应商首个启用模型。 */
  const onSelectCreateProvider = useCallback(
    (providerId: string) => {
      setSelectedProviderId(providerId);
      const provider = providerCatalog.find((entry) => entry.id === providerId) ?? null;
      const model = firstEnabledModel(provider);
      setSelectedModelId(model?.model_id ?? null);
      setModelDraft(model?.model_id ?? "");
    },
    [providerCatalog],
  );

  /** ADR-028 v0.9：旧会话补选供应商（仅 provider 为空时显示；带出首个模型）。 */
  const onSelectReselectProvider = useCallback(
    (providerId: string) => {
      setReselectProviderId(providerId || null);
      const provider = providerCatalog.find((entry) => entry.id === providerId) ?? null;
      const model = firstEnabledModel(provider);
      setReselectModelId(model?.model_id ?? null);
    },
    [providerCatalog],
  );

  /** ADR-028 v0.9：切换运行时 → 记忆预填（失效不自动回退；无记忆保持空选择）。 */
  const lastPrefillRuntime = useRef<string>("");
  useEffect(() => {
    if (autoRoute) {
      lastPrefillRuntime.current = "";
      return;
    }
    if (!providerCatalogLoaded || !selectedRuntime || lastPrefillRuntime.current === selectedRuntime) {
      return;
    }
    lastPrefillRuntime.current = selectedRuntime;
    const remembered = rememberedProvider(providerCatalog, selectedRuntime, providerMemory);
    if (remembered !== null) {
      setSelectedProviderId(remembered.id);
      const model = firstEnabledModel(remembered);
      if (model !== null) {
        setSelectedModelId(model.model_id);
        setModelDraft(model.model_id);
      }
    } else {
      // 供应商失效/无记忆：不自动选择（ADR-028：不得自动回退到其他 provider）。
      setSelectedProviderId(null);
      setSelectedModelId(null);
      setModelDraft("");
    }
  }, [autoRoute, providerCatalog, providerCatalogLoaded, providerMemory, selectedRuntime]);

  /** M3-11：手动输入模型（保留 UI-05 路径）；与选择器选择不一致时清空选择态。 */
  const onModelDraftChange = useCallback((value: string) => {
    setModelDraft(value);
    setSelectedModelId((current) => (current !== null && current !== value ? null : current));
    setSelectedProviderId((current) => (selectedModelId === value ? current : null));
  }, [selectedModelId]);

  const onCreateSession = useCallback(async () => {
    if (!autoRoute && !selectedRuntime) {
      setErrorCode(null);
      setError("请选择运行时或开启自动路由");
      return;
    }
    // M3-03 DoD3（D5）：`disabled` 运行时不可创建会话（选择器已禁用；此处兜底
    // 覆盖「选中后运行时转为 disabled」的窗口；后端另有等价防线）。自动路由不带
    // 运行时（后端按候选过滤排除禁用项；无候选回 `no_candidate_runtime`）。
    if (!autoRoute) {
      const selected = runtimes.find((runtime) => runtime.id === selectedRuntime);
      if (selected?.status === "disabled") {
        const reason = selected.status_reason ?? "disabled";
        setErrorCode("invalid_value");
        setError(`运行时 ${selected.name} 已禁用（${reason}），不可创建会话；请先修复并重新启用`);
        return;
      }
    }
    const title = titleDraft.trim() || `会话 ${new Date().toLocaleTimeString()}`;
    const model = modelDraft.trim();
    // ADR-028 v0.9：三官方运行时（以及自动路由）新建会话 provider + model 必选；
    // 无有效选择时前端先拒绝（后端仍为权威校验方）。
    const requiresProvider = autoRoute || requiresProviderSelection(selectedRuntime);
    if (requiresProvider) {
      if (!selectedProviderId) {
        setErrorCode(null);
        setError("请选择供应商");
        return;
      }
      if (!model) {
        setErrorCode(null);
        setError("请选择模型");
        return;
      }
      const provider = providerCatalog.find((entry) => entry.id === selectedProviderId);
      if (
        !provider ||
        !provider.enabled ||
        (!autoRoute && !providerSupportsRuntime(provider, selectedRuntime))
      ) {
        setErrorCode("invalid_value");
        setError("供应商不可用（已停用/已删除/不支持当前运行时协议）；请重新选择");
        return;
      }
    }
    // M7-09/ADR-026：所选预设（清单刷新后已消失的 id 不再透传，避免无谓的
    // `preset_not_found`；服务端仍为权威校验方）。
    const selectedPresetId =
      activePresetId && presets.some((preset) => preset.id === activePresetId)
        ? activePresetId
        : null;
    const startedAt = performance.now();
    try {
      const created = await ipc.createSession({
        // M7-02/ADR-022：缺省 = 自动路由（不带 `runtime_id`）；显式 = 手动优先。
        ...(autoRoute ? {} : { runtime_id: selectedRuntime }),
        title,
        ...(model ? { model } : {}),
        // ADR-028 v0.9：显式消歧 provider + model（三官方运行时/自动路由必选；
        // 其他运行时仅在用户显式选择时透传，由核心按能力校验）。
        ...(selectedProviderId ? { provider_id: selectedProviderId } : {}),
        // M3-10/ADR-010：会话级思考深度随 `session_create` 透传（缺省 2）。
        thinking_depth: thinkingDepth,
        // M6-01/ADR-014：会话级权限模式随 `session_create` 透传（缺省 workspace_write）。
        permission_mode: permissionMode,
        // M7-09/ADR-026：预设随 `session_create` 透传（合成提示词由核心完成）。
        ...(selectedPresetId ? { preset_id: selectedPresetId } : {}),
      });
      // ADR-028 v0.9：首次成功创建后写入全局记忆（runtime → provider；跨工作区）。
      if (requiresProvider && selectedProviderId) {
        const runtimeKey = created.runtime_id || selectedRuntime;
        if (runtimeKey) {
          const next = { ...providerMemory, [runtimeKey]: selectedProviderId };
          setProviderMemory(next);
          void settingsIpcProp
            .set(UI_PROVIDER_SELECTION_KEY, JSON.stringify(next))
            .catch(() => {
              // 记忆写入失败不阻断创建（下次需重新选择）。
            });
        }
      }
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
      // M7-04：新建会话即打开标签（并聚焦），布局持久化。
      applyLayout(openTab(layout, created.id));
      await refreshSessions();
    } catch (failure) {
      reportError(failure);
    }
  }, [
    applyLayout,
    activePresetId,
    autoRoute,
    ipc,
    layout,
    modelDraft,
    permissionMode,
    presets,
    providerCatalog,
    providerMemory,
    refreshSessions,
    reportError,
    runtimes,
    settingsIpcProp,
    selectedRuntime,
    selectedProviderId,
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
    // ADR-028 v0.9：只读（provider 失效）禁止发送；旧会话 provider/model 补选经
    // `session_send.provider_id/model` 承载（不新增命令）。
    if (activeRequiresProvider && activeSession?.provider_state === "invalid") {
      sendInFlight.current = false;
      setSending(false);
      setErrorCode("invalid_value");
      setError("供应商已停用/删除：当前会话只读（可中断在途任务），请重选供应商或新建会话");
      return;
    }
    const selection: { provider_id?: string; model?: string } = {};
    if (activeRequiresProvider && !activeSession?.provider_id) {
      if (reselectProviderId) {
        selection.provider_id = reselectProviderId;
      }
    }
    if (
      activeRequiresProvider &&
      activeSession &&
      (!activeSession.provider_id || activeSession.model_state !== "active")
    ) {
      if (reselectModelId) {
        selection.model = reselectModelId;
      }
    }
    try {
      const ack = await ipc.sendMessage({
        session_id: activeSessionId,
        text,
        client_msg_id: generateUlid(),
        ...selection,
        // M3-10/ADR-010：本次 run 覆盖（仅本次 run，不回写会话级值）。
        thinking_depth: thinkingDepth,
        // M7-06/ADR-024：随消息附带的文件引用（空数组省略字段）。
        ...(pendingReferences.length > 0 ? { references: pendingReferences } : {}),
      });
      setThinkingWarning(thinkingWarningOf(ack.warnings));
      setDraft("");
      setReselectProviderId(null);
      setReselectModelId(null);
      // M7-06：引用为一次性上下文（不落库、不跨 run；发送成功后清空）。
      setPendingReferences([]);
      setLastAck({
        runId: ack.run_id,
        duplicate: ack.duplicate,
        sessionId: activeSessionId,
        // M7-02/ADR-022：超闸门排队位置（无排队省略）。
        queue: ack.queue ?? null,
      });
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
    activeRequiresProvider,
    activeSession,
    activeSessionId,
    clearError,
    draft,
    ipc,
    pendingReferences,
    refreshSessions,
    reportError,
    reselectModelId,
    reselectProviderId,
    thinkingDepth,
  ]);

  /** M7-06/ADR-024：添加引用（去重；上限 32 条，与核心命令层同值）。 */
  const addReference = useCallback((path: string, kind: SessionReference["kind"]) => {
    const trimmed = path.trim();
    if (trimmed.length === 0) {
      return;
    }
    setPendingReferences((previous) => {
      if (previous.length >= MAX_SESSION_REFERENCES) {
        return previous;
      }
      if (previous.some((reference) => reference.path === trimmed)) {
        return previous;
      }
      return [...previous, { path: trimmed, kind }];
    });
  }, []);

  const removeReference = useCallback((path: string) => {
    setPendingReferences((previous) =>
      previous.filter((reference) => reference.path !== path),
    );
  }, []);

  /**
   * M7-06/ADR-024：`@` 入口（空输入框按 `@` → 系统文件选择器 → 添加引用）。
   * 输入框非空时 `@` 按普通字符输入（不劫持）。
   */
  const onPickReference = useCallback(async () => {
    try {
      const picked = await artifactsIpcProp.pick("file");
      if (picked) {
        addReference(picked, "file");
      }
    } catch (failure) {
      reportError(failure);
    }
  }, [addReference, artifactsIpcProp, reportError]);

  /**
   * M7-06/ADR-024：拖拽注入（DOM `drop` + preload `pathForFile`；ADR-029 §7）。
   * 命中输入区即接收；非 Electron 环境（vitest/jsdom）无 `window.aether` 时静默跳过。
   */
  const onComposerDragOver = useCallback((event: DragEvent<HTMLDivElement>) => {
    if (event.dataTransfer?.types.includes("Files")) {
      event.preventDefault();
    }
  }, []);

  const onComposerDrop = useCallback(
    (event: DragEvent<HTMLDivElement>) => {
      const files = Array.from(event.dataTransfer?.files ?? []);
      if (files.length === 0) {
        return;
      }
      event.preventDefault();
      const container = composerRef.current;
      if (container) {
        const rect = container.getBoundingClientRect();
        if (
          event.clientX < rect.left ||
          event.clientX > rect.right ||
          event.clientY < rect.top ||
          event.clientY > rect.bottom
        ) {
          return;
        }
      }
      for (const file of files) {
        const path = window.aether?.pathForFile(file);
        if (path) {
          addReference(path, kindForPath(path));
        }
      }
    },
    [addReference],
  );

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

  /**
   * M7-03/ADR-017：暂停/恢复切换（`session_pause` / `session_resume`）。
   *
   * 暂停 = 核心层受控中断（在途 run 收口 `cancelled(user_pause)`）+ `paused` 门禁态；
   * 恢复 = 有暂停标记时重放被中断输入（Mode R 原生上下文），否则 `paused → idle`。
   * 能力未声明/连接未就绪 → `pause_unsupported`（按钮已置灰；后端为硬门）。
   */
  const onTogglePause = useCallback(async () => {
    if (!activeSessionId || !activeSession || pauseBusy) {
      return;
    }
    setPauseBusy(true);
    clearError();
    try {
      if (activeSession.status === "paused") {
        await ipc.resumeSession(activeSessionId);
      } else {
        await ipc.pauseSession(activeSessionId);
      }
      await refreshSessions();
    } catch (failure) {
      reportError(failure);
    } finally {
      setPauseBusy(false);
    }
  }, [
    activeSession,
    activeSessionId,
    clearError,
    ipc,
    pauseBusy,
    refreshSessions,
    reportError,
  ]);

  /** M5-05：单会话导出（缺省写入数据目录 `archives/`；锚点 `session-export`）。 */
  const onExportSession = useCallback(async () => {
    if (!activeSessionId || exportBusy) {
      return;
    }
    setExportBusy(true);
    setExportResult(null);
    try {
      const response = await archiveIpcProp.exportSession(activeSessionId);
      setExportResult({
        result: "ok",
        message: `已导出：${response.export.path}（${response.export.rows} 行，sha256 ${response.export.sha256.slice(0, 12)}…）`,
      });
    } catch (failure) {
      setExportResult({ result: "failed", message: describeIpcError(failure) });
    } finally {
      setExportBusy(false);
    }
  }, [activeSessionId, archiveIpcProp, exportBusy]);

  /** M3-06：一键重放（`run_retry`；仅终态 run 由后端判定；新 run 事件流驱动渲染）。 */
  const onRetryRun = useCallback(
    async (runId: string) => {
      if (retryingRunId !== null) {
        return;
      }
      if (
        activeRequiresProvider &&
        (activeProviderState === "invalid" || activeModelState !== "active")
      ) {
        setErrorCode("invalid_value");
        setError("供应商失效或模型失效/缺失：请先补选后再重试");
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
    [
      activeModelState,
      activeProviderState,
      activeRequiresProvider,
      activeSession,
      clearError,
      ipc,
      refreshSessions,
      reportError,
      retryingRunId,
    ],
  );

  const interruptible =
    projection.activeRunId !== null || activeSession?.status === "running";
  const runStatusText = projection.activeRunId
    ? "run 运行中"
    : (projection.runs.at(-1)?.status ?? "无进行中 run");

  return (
    <section
      className={sideCollapsed ? "workbench side-collapsed" : "workbench"}
      data-testid="workbench"
      data-mode={mode}
    >
      {/* T1 埋点锚点（M4-04 读取；`data-ms` 为空表示尚未创建会话）。 */}
      <span
        hidden
        data-testid="create-latency-ms"
        data-ms={createLatencyMs === null ? "" : String(createLatencyMs)}
      />

      {/* ===== 左侧栏（原型 .side：模式切换 / 新建 / 搜索 / 工作区 / 会话 / 用户） ===== */}
      <aside className={sideCollapsed ? "side collapsed" : "side"} data-testid="sidebar">
        <div className="side-inner">
          <div className="side-top">
            <div className="mode-row">
              <div className="mode" role="tablist" aria-label="工作模式">
                <button
                  type="button"
                  role="tab"
                  aria-selected={mode === "agent"}
                  className={mode === "agent" ? "on" : ""}
                  data-testid="mode-agent"
                  onClick={() => setMode("agent")}
                >
                  ◈ Agent
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={mode === "chat"}
                  className={mode === "chat" ? "on" : ""}
                  data-testid="mode-chat"
                  onClick={() => setMode("chat")}
                >
                  ▢ Chat
                </button>
              </div>
              <button
                type="button"
                className="side-collapse"
                title="折叠侧栏"
                aria-label="折叠侧栏"
                data-testid="side-collapse"
                onClick={() => setSideCollapsed(true)}
              >
                <svg viewBox="0 0 24 24">
                  <rect x="3" y="4" width="18" height="16" rx="2" />
                  <path d="M9 4v16" />
                </svg>
              </button>
            </div>
            <button
              type="button"
              className={createOpen ? "newbtn active" : "newbtn"}
              data-testid="new-session-open"
              aria-expanded={createOpen}
              onClick={() => setCreateOpen((open) => !open)}
            >
              ＋ 新建会话
            </button>
          </div>

          <div className="ws-list">
            <div className="workbench-sidebar side-sessions">
          {/* M7-08/ADR-026 §2.1：工作区树（分组/计数/折叠 + 创建/改名/删除/改派；
              锚点 `workspace-tree` / `workspace-item` / `workspace-create`）。 */}
          <WorkspaceTree
            ipc={workspacesIpcProp}
            sessions={sessions}
            activeSessionId={activeSessionId}
            onOpenSession={openSessionView}
            onAssigned={() => {
              void refreshSessionsRef.current();
            }}
          />
          <h2 className="workbench-title">会话</h2>
          {/* M7-07/ADR-025 §2.1-2：会话搜索（Ctrl+K；锚点 `session-search` /
              `session-search-result`）。空查询显示常规分组列表。 */}
          <div className="session-search">
            <svg viewBox="0 0 24 24" aria-hidden="true">
              <circle cx="11" cy="11" r="7" />
              <path d="M21 21l-4.35-4.35" />
            </svg>
            <input
              ref={searchInputRef}
              type="search"
              data-testid="session-search"
              aria-label="搜索会话"
              placeholder="搜索会话（Ctrl+K）"
              value={searchQuery}
              onChange={(event) => setSearchQuery(event.target.value)}
            />
            {searchQuery ? (
              <button
                type="button"
                data-testid="session-search-clear"
                onClick={() => setSearchQuery("")}
              >
                清除
              </button>
            ) : null}
          </div>
          {searchBusy ? (
            <div className="session-search-loading" data-testid="session-search-loading">
              搜索中…
            </div>
          ) : null}
          {manageError ? (
            <div
              className="session-manage-error"
              data-testid="session-manage-error"
              data-code={manageError.code ?? ""}
            >
              {manageError.message}
            </div>
          ) : null}
          {searchHits !== null ? (
            <ul className="session-search-results" data-testid="session-search-results">
              {searchHits.map((hit) => (
                <li key={hit.id}>
                  <button
                    type="button"
                    data-testid="session-search-result"
                    data-session-id={hit.id}
                    data-source={hit.source}
                    className={
                      hit.id === activeSessionId
                        ? "session-search-result active"
                        : "session-search-result"
                    }
                    onClick={() => openSessionView(hit.id)}
                  >
                    <span className="session-item-title">{hit.title}</span>
                    <span className="session-search-snippet">{hit.snippet}</span>
                    <span className="session-search-source">
                      {hit.source === "title" ? "标题命中" : "正文命中"}
                    </span>
                  </button>
                </li>
              ))}
              {searchHits.length === 0 ? (
                <li className="session-empty" data-testid="session-search-empty">
                  无匹配会话
                </li>
              ) : null}
            </ul>
          ) : (
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
                      <li key={session.id} className="session-row">
                        {renamingSessionId === session.id ? (
                          <div className="session-rename-form" data-testid="session-rename-form">
                            <input
                              data-testid="session-rename-input"
                              aria-label="会话新标题"
                              value={renameDraft}
                              autoFocus
                              onChange={(event) => setRenameDraft(event.target.value)}
                              onKeyDown={(event) => {
                                if (event.key === "Enter") {
                                  event.preventDefault();
                                  void submitRename();
                                }
                                if (event.key === "Escape") {
                                  setRenamingSessionId(null);
                                }
                              }}
                            />
                            <button
                              type="button"
                              data-testid="session-rename-save"
                              disabled={renameBusy}
                              onClick={() => void submitRename()}
                            >
                              保存
                            </button>
                            <button
                              type="button"
                              data-testid="session-rename-cancel"
                              onClick={() => setRenamingSessionId(null)}
                            >
                              取消
                            </button>
                          </div>
                        ) : (
                          <button
                            type="button"
                            data-testid="session-item"
                            data-session-id={session.id}
                            data-active={String(session.id === activeSessionId)}
                            data-status={session.status}
                            className={
                              session.id === activeSessionId ? "session-item active" : "session-item"
                            }
                            onClick={() => openSessionView(session.id)}
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
                        )}
                        <div className="session-item-actions">
                          <button
                            type="button"
                            data-testid="session-rename"
                            data-session-id={session.id}
                            onClick={() => {
                              setDeletingSessionId(null);
                              setRenamingSessionId(session.id);
                              setRenameDraft(session.title);
                            }}
                          >
                            改名
                          </button>
                          <button
                            type="button"
                            data-testid="session-delete"
                            data-session-id={session.id}
                            onClick={() => {
                              setRenamingSessionId(null);
                              setDeletingSessionId(session.id);
                            }}
                          >
                            删除
                          </button>
                        </div>
                        {deletingSessionId === session.id ? (
                          <div
                            className="session-delete-confirm-box"
                            data-testid="session-delete-confirm-box"
                          >
                            <span className="session-delete-warning">
                              删除后不可恢复：消息/运行/引用/审批记录将级联删除。
                            </span>
                            <button
                              type="button"
                              data-testid="session-delete-confirm"
                              disabled={deleteBusy}
                              onClick={() => void confirmDelete()}
                            >
                              确认删除
                            </button>
                            <button
                              type="button"
                              data-testid="session-delete-cancel"
                              onClick={() => setDeletingSessionId(null)}
                            >
                              取消
                            </button>
                          </div>
                        ) : null}
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
          )}

          <div
            className={createOpen ? "session-create" : "session-create collapsed"}
            data-testid="session-create-form"
            data-open={String(createOpen)}
          >
            <p className="session-create-runtime">
              Agent：{autoRoute ? "自动路由" : selectedRuntimeInfo?.name ?? "未选择（在输入区 Agent 选择器切换）"}
            </p>
            <input
              type="text"
              data-testid="session-title-input"
              placeholder="会话标题"
              value={titleDraft}
              onChange={(event) => setTitleDraft(event.target.value)}
            />
            {createRequiresProvider ? (
              <select
                data-testid="session-provider-select"
                data-context="create"
                data-provider-id={selectedProviderId ?? ""}
                data-disabled={String(availableCreateProviders.length === 0)}
                data-invalid={String(
                  selectedProviderId !== null &&
                    !availableCreateProviders.some(
                      (provider) => provider.id === selectedProviderId,
                    ),
                )}
                aria-label="供应商"
                value={selectedProviderId ?? ""}
                disabled={availableCreateProviders.length === 0}
                onChange={(event) => onSelectCreateProvider(event.target.value)}
              >
                <option value="">
                  {availableCreateProviders.length === 0
                    ? "无可用供应商（请先在设置中配置并启用）"
                    : "选择供应商"}
                </option>
                {availableCreateProviders.map((provider) => (
                  <option key={provider.id} value={provider.id}>
                    {provider.name}
                  </option>
                ))}
              </select>
            ) : null}
            {createProviderMissing ? (
              <p
                className="provider-required-notice"
                data-testid="provider-required-notice"
                data-state="missing"
                data-field="provider"
                role="status"
              >
                请选择供应商
              </p>
            ) : createModelMissing ? (
              <p
                className="provider-required-notice"
                data-testid="provider-required-notice"
                data-state="missing"
                data-field="model"
                role="status"
              >
                请选择模型
              </p>
            ) : null}
            <input
              type="text"
              data-testid="session-model-input"
              placeholder={createRequiresProvider ? "模型（必选）" : "会话级模型（可选，覆盖全局默认）"}
              value={modelDraft}
              onChange={(event) => onModelDraftChange(event.target.value)}
            />
            {/* M7-09/ADR-026：新建会话入口可选预设（合成提示词由核心按
                「预设块 → 工作区约定块」完成；失败不阻断会话创建以外的路径）。 */}
            <select
              data-testid="session-preset-select"
              aria-label="Agent 预设"
              value={activePresetId ?? ""}
              onChange={(event) =>
                onActivePresetChange?.(event.target.value === "" ? null : event.target.value)
              }
            >
              <option value="">不使用预设</option>
              {presets.map((preset) => (
                <option key={preset.id} value={preset.id}>
                  {preset.is_builtin ? `${preset.name}（内置）` : preset.name}
                </option>
              ))}
            </select>
            <button
              type="button"
              data-testid="session-create-submit"
              disabled={createProviderMissing || createModelMissing}
              onClick={onCreateSession}
            >
              新建会话
            </button>
          </div>
            </div>
          </div>

          {statusSlot ? <div className="side-status">{statusSlot}</div> : null}

          <div className="user-area">
            <button
              type="button"
              className="user-btn"
              data-testid="user-menu-toggle"
              aria-expanded={userMenuOpen}
              onClick={() => setUserMenuOpen((open) => !open)}
            >
              <span className="avatar">用</span>
              <span className="nm">用户</span>
              <span className="ge">⚙</span>
            </button>
            <div
              className={userMenuOpen ? "user-menu show" : "user-menu"}
              data-testid="user-menu"
            >
              {onOpenSettings ? (
                <button
                  type="button"
                  data-testid="settings-open"
                  onClick={() => {
                    setUserMenuOpen(false);
                    onOpenSettings();
                  }}
                >
                  ⚙ &nbsp;设置
                </button>
              ) : null}
              {onOpenBackup ? (
                <button
                  type="button"
                  data-testid="backup-open"
                  onClick={() => {
                    setUserMenuOpen(false);
                    onOpenBackup();
                  }}
                >
                  ⛁ &nbsp;备份与恢复
                </button>
              ) : null}
              {onOpenDiagnostics ? (
                <button
                  type="button"
                  data-testid="diagnostics-open"
                  onClick={() => {
                    setUserMenuOpen(false);
                    onOpenDiagnostics();
                  }}
                >
                  ⤓ &nbsp;诊断
                </button>
              ) : null}
              {onOpenAudit ? (
                <button
                  type="button"
                  data-testid="audit-open"
                  onClick={() => {
                    setUserMenuOpen(false);
                    onOpenAudit();
                  }}
                >
                  ☷ &nbsp;审计
                </button>
              ) : null}
              {onOpenAbout ? (
                <button
                  type="button"
                  data-testid="about-open"
                  onClick={() => {
                    setUserMenuOpen(false);
                    onOpenAbout();
                  }}
                >
                  ◈ &nbsp;关于
                </button>
              ) : null}
              {appVersion || protocolVersion || dataDir ? (
                <>
                  <hr />
                  <div className="user-meta">
                    {appVersion ? (
                      <span data-testid="app-version">版本 {appVersion}</span>
                    ) : null}
                    {protocolVersion ? (
                      <span data-testid="protocol-version">
                        线协议 v{protocolVersion.major}.{protocolVersion.minor}
                      </span>
                    ) : null}
                    {dataDir ? (
                      <span data-testid="app-data-dir" title={dataDir}>
                        数据目录：{dataDir}
                      </span>
                    ) : null}
                  </div>
                </>
              ) : null}
            </div>
          </div>
        </div>
      </aside>

      {sideCollapsed ? (
        <button
          type="button"
          className="side-reopen show"
          title="展开侧栏"
          aria-label="展开侧栏"
          data-testid="side-reopen"
          onClick={() => setSideCollapsed(false)}
        >
          <svg viewBox="0 0 24 24">
            <rect x="3" y="4" width="18" height="16" rx="2" />
            <path d="M9 4v16" />
          </svg>
        </button>
      ) : null}

      {/* ===== 中栏（原型 .center：会话头 / 标签 / 消息流 / 输入区） ===== */}
      <div className="center">
        <TopBar
          session={activeSession}
          runId={projection.activeRunId}
          runStatusText={runStatusText}
          interruptible={interruptible}
          onInterrupt={onInterrupt}
          lastAck={lastAck}
          runtimes={runtimes}
          pendingCount={pendingPermissionCount}
          onExportSession={activeSession ? () => void onExportSession() : undefined}
          exportBusy={exportBusy}
          exportResult={exportResult?.result ?? null}
          // M7-03/ADR-017：暂停/恢复（锚点 `session-pause-toggle`；有会话时渲染）。
          pauseSupported={pauseSupported}
          pauseDisabled={pauseDisabled}
          onTogglePause={activeSession ? () => void onTogglePause() : undefined}
          pauseBusy={pauseBusy}
        />
        {/* M7-04：标签页（UI-01 完整；锚点 `workbench-tabs` / `session-tab` /
            `session-tab-badge` / `session-tab-close` / `split-toggle`）。 */}
        <WorkbenchTabs
          sessions={tabSessions}
          activeSessionId={activeSessionId}
          splitSessionId={splitSession?.id ?? null}
          onActivate={onActivateTab}
          onClose={onCloseTab}
          onToggleSplit={onToggleSplit}
          splitEnabled={splitEnabled}
        />

        <div className="chat-ph" data-testid="chat-placeholder">
          <div className="chat-card">
            <div className="em">◫</div>
            <h3>多 Agent 聊天室</h3>
            <p>同时与多个 Agent 对话、@mention 路由、多运行时协作。</p>
            <p>
              当前阶段：<b className="chat-phase">P2 路线图</b> · 尚未开放
            </p>
            <div className="tag">P0 占位 · 切换回 Agent 继续工作</div>
          </div>
        </div>

        <div className="workbench-main-pair" data-split={String(splitSession !== null)}>
          <div
            className="workbench-main"
            data-testid="pane-primary"
            data-session-id={activeSessionId ?? ""}
          >
          {!activeSession ? (
            <p className="workbench-hint" data-testid="no-session-hint">
              请选择或新建会话
            </p>
          ) : null}

          {activeSession && activeRequiresProvider ? (
            <div className="session-provider-bar">
              <span
                data-testid="session-provider-current"
                data-provider-id={activeSession.provider_id ?? ""}
                data-state={
                  activeProviderState === "invalid"
                    ? "readonly"
                    : activeProviderState === "active"
                      ? "active"
                      : "invalid"
                }
                data-model-state={activeModelState}
              >
                供应商：{activeSession.provider_name ?? activeSession.provider_id ?? "未选择"} · 模型：
                {activeSession.model ?? "未选择"}
              </span>
              {activeProviderState === "invalid" ? (
                <p
                  className="session-provider-readonly"
                  data-testid="session-provider-readonly"
                  data-reason={
                    activeSession.provider_id
                      ? activeSession.provider_name
                        ? "provider_disabled"
                        : "provider_deleted"
                      : "provider_missing"
                  }
                  role="alert"
                >
                  供应商已停用/删除：当前会话只读（可查看历史、可中断在途任务）；禁止发送/重试/恢复及补选。
                </p>
              ) : null}
              {activeProviderState !== "invalid" && !activeSession.provider_id ? (
                <>
                  <select
                    data-testid="session-provider-select"
                    data-context="reselect"
                    data-provider-id={reselectProviderId ?? ""}
                    data-busy={selectionRunInFlight ? "run_in_flight" : undefined}
                    data-disabled={String(selectionRunInFlight)}
                    aria-label="补选供应商"
                    value={reselectProviderId ?? ""}
                    disabled={selectionRunInFlight || reselectOptions.length === 0}
                    onChange={(event) => onSelectReselectProvider(event.target.value)}
                  >
                    <option value="">
                      {reselectOptions.length === 0 ? "无可用供应商" : "补选供应商"}
                    </option>
                    {reselectOptions.map((provider) => (
                      <option key={provider.id} value={provider.id}>
                        {provider.name}
                      </option>
                    ))}
                  </select>
                  {!reselectProviderId ? (
                    <p
                      className="provider-required-notice"
                      data-testid="provider-required-notice"
                      data-state="missing"
                      data-field="provider"
                      role="status"
                    >
                      请选择供应商
                    </p>
                  ) : null}
                </>
              ) : null}
              {activeProviderState !== "invalid" &&
              activeSession.provider_id &&
              activeModelState !== "active" ? (
                <>
                  <select
                    data-testid="session-model-reselect"
                    data-model-state={activeModelState}
                    data-busy={selectionRunInFlight ? "run_in_flight" : undefined}
                    aria-label="补选模型"
                    value={reselectModelId ?? ""}
                    disabled={selectionRunInFlight || activeProviderModels.length === 0}
                    onChange={(event) => setReselectModelId(event.target.value || null)}
                  >
                    <option value="">
                      {activeProviderModels.length === 0 ? "无可用模型" : "补选模型"}
                    </option>
                    {activeProviderModels.map((model) => (
                      <option key={model.id} value={model.model_id}>
                        {model.display_name}
                      </option>
                    ))}
                  </select>
                  {!reselectModelId ? (
                    <p
                      className="provider-required-notice"
                      data-testid="provider-required-notice"
                      data-state="missing"
                      data-field="model"
                      role="status"
                    >
                      请选择模型
                    </p>
                  ) : null}
                </>
              ) : null}
              {selectionRunInFlight &&
              !activeSession.provider_id &&
              reselectProviderId ? (
                <p className="provider-required-notice" data-state="invalid" data-field="provider">
                  当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。
                </p>
              ) : null}
            </div>
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

          <div className="msgs message-stream" data-testid="message-stream">
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
                // 失败原因（适配器 run.failed.message；如 Claude `api_error` 的
                // api_error_status / 服务端返回详情）。展示截断文本，完整内容见 title。
                const errorDetail = runView?.errorMessage?.replace(/\s+/g, " ").trim() ?? "";
                const errorDetailShort =
                  errorDetail.length > 160 ? `${errorDetail.slice(0, 160)}…` : errorDetail;
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
                    {bubble.role === "user" ? (
                      <span className="message-meta">已发送</span>
                    ) : null}
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
                          title={errorDetail.length > 0 ? errorDetail : undefined}
                        >
                          {runView?.status === "failed"
                            ? `运行失败：${runView.errorCode ?? "run_failed"}${
                                errorDetailShort ? ` · ${errorDetailShort}` : ""
                              }`
                            : interruptedByDegraded
                              ? "已中断（存储降级）"
                              : "已中断"}
                        </span>
                        <button
                          type="button"
                          data-testid="run-retry"
                          data-run-id={bubble.runId}
                          // M7-03/ADR-017：paused 态 `run_retry` 被拒（session_paused）→ 入口禁用。
                          disabled={
                            degraded ||
                            retryingRunId !== null ||
                            activeSession?.status === "paused" ||
                            (activeRequiresProvider &&
                              (activeProviderState === "invalid" ||
                                activeModelState !== "active"))
                          }
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

          {exportResult ? (
            <p
              className="session-export-result"
              data-testid="session-export-result"
              data-result={exportResult.result}
              role={exportResult.result === "failed" ? "alert" : "status"}
            >
              {exportResult.message}
            </p>
          ) : null}

          {/* M7-02/ADR-022：闸门排队位置（最近发送 ack 的 `queue` 快照；run 派发
              （`run.started`）后隐藏）。`data-position` / `data-limit` 供断言。 */}
          {queueNotice ? (
            <p
              className="queue-position"
              data-testid="queue-position"
              data-position={queueNotice.position}
              data-limit={queueNotice.limit}
              role="status"
            >
              排队中：第 {queueNotice.position} 位（并发上限 {queueNotice.limit}）
            </p>
          ) : null}

          {/* M7-03/ADR-017：paused 门禁提示（状态条提示 + 常驻语义声明；UI-UX §5/§2.4）。 */}
          {activeSession?.status === "paused" ? (
            <p className="session-paused-notice" data-testid="paused-notice" role="status">
              {PAUSED_STATUS_HINT}；{PAUSE_SEMANTICS_NOTICE}
            </p>
          ) : null}

          {/* 原型：权限请求以卡片形式出现在会话流中（常驻挂载以同步 pending 计数）。 */}
          <div
            className="permission-inline"
            data-empty={String(!activeSessionId || pendingPermissionCount === 0)}
          >
            <PermissionPanel
              ipc={permissionIpc}
              sessionId={activeSessionId}
              events={eventsState.events}
              onPendingCount={setPendingPermissionCount}
            />
          </div>

          <div className="input-area">
          <div
            className="composer input-box"
            data-testid="composer"
            ref={composerRef}
            onDragOver={onComposerDragOver}
            onDrop={onComposerDrop}
          >
            {/* M7-06/ADR-024 A-3：随消息附带的文件引用（拖拽 / `@` 入口；锚点
                `file-reference`）。**引用 ≠ 预授权**：Agent 读取仍走 fs.read 权限门。 */}
            <div
              className="composer-references"
              data-testid="file-reference"
              data-count={pendingReferences.length}
            >
              {pendingReferences.length === 0 ? (
                <span className="composer-references-hint">
                  引用：拖入文件，或在空输入框按 @ 选择
                </span>
              ) : (
                <ul className="composer-reference-list">
                  {pendingReferences.map((reference) => (
                    <li
                      key={reference.path}
                      className="composer-reference-chip"
                      data-testid="file-reference-chip"
                      data-path={reference.path}
                      data-kind={reference.kind}
                    >
                      <span className="composer-reference-name">
                        {reference.path.split(/[\\/]/).pop() ?? reference.path}
                      </span>
                      <button
                        type="button"
                        data-testid="file-reference-remove"
                        data-path={reference.path}
                        disabled={sending}
                        onClick={() => removeReference(reference.path)}
                        aria-label={`移除引用 ${reference.path}`}
                      >
                        ×
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <textarea
              data-testid="composer-input"
              placeholder="输入消息…（@ 引用文件；Ctrl+Enter 发送）"
              value={draft}
              // M7-03/ADR-017：paused 态发送被拒（session_paused，草稿保留；UI-UX §3.3）。
              disabled={!activeSessionId || degraded || activeSession?.status === "paused"}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
                  event.preventDefault();
                  void onSend();
                  return;
                }
                // M7-06/ADR-024：`@` 入口（空输入框按 @ → 文件选择器；非空不劫持）。
                if (event.key === "@" && draft.length === 0 && !event.ctrlKey && !event.metaKey) {
                  event.preventDefault();
                  void onPickReference();
                }
              }}
            />
            <div className="input-bar composer-toolbar">
              {/* M3-10/ADR-010：思考深度 5 档滑块（关闭/低/高/极高/最高，默认高）；
                  运行时未声明 `thinking_depth` 能力时滑块置灰 + tooltip。 */}
              <div className="thinking-control">
                <button
                  type="button"
                  className={thinkingSupported ? "thinking-toggle ic think-on" : "thinking-toggle ic"}
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
                  <span className="ic-label" data-testid="thinking-current">
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
                    className="thinking-popover ipop ipop-think"
                    data-testid="thinking-popover"
                    data-enabled={thinkingSupported}
                  >
                    <div className="thinking-head">
                      <span>思考深度</span>
                      <span className="thinking-value">
                        {THINKING_DEPTH_LABELS[thinkingDepth]}
                      </span>
                    </div>
                    <div className="thinking-range">
                      <div
                        className="thinking-bubble"
                        style={{
                          left: `calc(${thinkingPercent}% + ${(50 - thinkingPercent) * 0.16}px)`,
                        }}
                        aria-hidden="true"
                      >
                        {THINKING_DEPTH_LABELS[thinkingDepth]}
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
                        aria-valuetext={THINKING_DEPTH_LABELS[thinkingDepth]}
                        style={{
                          background: `linear-gradient(to right, var(--ac) ${thinkingPercent}%, var(--bd2) ${thinkingPercent}%)`,
                        }}
                        onChange={(event) => setThinkingDepth(Number(event.target.value))}
                      />
                    </div>
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
              {/* M6-01/ADR-014：会话级权限模式选择器（无激活会话禁用 + 空态；
                  常驻边界声明 `permission-mode-boundary` 在弹层内，见组件）。 */}
              <PermissionModeSelector
                ipc={ipc}
                sessionId={activeSessionId}
                value={activeSessionId ? permissionMode : null}
                onChange={(mode) => void onChangePermissionMode(mode)}
                disabled={degraded}
              />
              <button
                type="button"
                className="ic"
                data-testid="composer-attach"
                title="添加文件引用（也可在空输入框按 @）"
                aria-label="添加文件引用"
                disabled={!activeSessionId || degraded}
                onClick={() => void onPickReference()}
              >
                <svg viewBox="0 0 24 24">
                  <path d="M21 11l-9 9a6 6 0 01-8.5-8.5l9-9a4 4 0 015.5 5.5l-9 9a2 2 0 01-2.8-2.8l8-8" />
                </svg>
              </button>
              <div className="input-spacer" />
              {/* 原型 agent picker：运行时选择 + 运行时状态管理（常驻 DOM，未展开时隐藏）。 */}
              <div className="picker agent-picker" data-testid="agent-picker">
                <button
                  type="button"
                  className="picker-btn"
                  data-testid="agent-picker-toggle"
                  aria-expanded={agentPickerOpen}
                  title="选择 Agent 运行时"
                  onClick={() => setAgentPickerOpen((open) => !open)}
                >
                  <span
                    className="picker-mi"
                    style={{ background: autoRoute ? "var(--ac)" : runtimeAccentOf(selectedRuntimeInfo) }}
                  >
                    {autoRoute ? "A" : runtimeAbbrOf(selectedRuntimeInfo)}
                  </span>
                  <span className="agent-picker-label">
                    {autoRoute ? "自动路由" : selectedRuntimeInfo?.name ?? "选择 Agent"}
                  </span>
                  <span className="ar">▾</span>
                </button>
                <div
                  className={agentPickerOpen ? "dropdown agent-dd show" : "dropdown agent-dd"}
                  data-testid="agent-picker-menu"
                >
                  <div className="agent-dd-head">Agent 运行时</div>
                  <button
                    type="button"
                    role="switch"
                    aria-checked={autoRoute}
                    className={autoRoute ? "agent-auto-route on" : "agent-auto-route"}
                    data-testid="runtime-auto-route"
                    data-auto={String(autoRoute)}
                    onClick={() => setAutoRoute((value) => !value)}
                  >
                    <span>自动路由（按固定优先级）</span>
                    {autoRoute ? <span className="agent-check">✓</span> : null}
                  </button>
                  <div
                    className="runtime-selector agent-list"
                    data-testid="runtime-selector"
                    role="radiogroup"
                  >
                    {runtimes.map((runtime) => (
                      <button
                        key={runtime.id}
                        type="button"
                        role="radio"
                        aria-checked={!autoRoute && runtime.id === selectedRuntime}
                        data-testid="runtime-option"
                        data-runtime-id={runtime.id}
                        data-status={runtime.status}
                        data-selected={String(!autoRoute && runtime.id === selectedRuntime)}
                        disabled={runtime.status === "disabled"}
                        className={
                          runtime.id === selectedRuntime && !autoRoute
                            ? "runtime-option agent-option selected"
                            : "runtime-option agent-option"
                        }
                        onClick={() => {
                          setAutoRoute(false);
                          setSelectedRuntime(runtime.id);
                          setAgentPickerOpen(false);
                        }}
                      >
                        <span
                          className="picker-mi"
                          style={{ background: runtimeAccentOf(runtime) }}
                        >
                          {runtimeAbbrOf(runtime)}
                        </span>
                        <span className="runtime-name">{runtime.name}</span>
                        <span className="runtime-status">
                          {RUNTIME_STATUS_LABELS[runtime.status]}
                        </span>
                        {runtime.status_reason ? (
                          <span className="runtime-reason" data-testid="runtime-reason">
                            {runtime.status_reason}
                          </span>
                        ) : null}
                        {runtime.id === selectedRuntime && !autoRoute ? (
                          <span className="agent-check">✓</span>
                        ) : null}
                        <span
                          className="runtime-capabilities"
                          data-testid="runtime-capabilities"
                        >
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
                  <hr />
                  <details className="agent-runtime-details">
                    <summary>运行时状态</summary>
                    <RuntimePanel
                      runtimes={runtimes}
                      ipc={permissionIpc}
                      onRefresh={refreshRuntimes}
                    />
                  </details>
                </div>
              </div>
              {/* M3-11：输入区模型选择器（仅启用供应商的启用模型；作用于新建会话）。 */}
              <ModelSelector
                ipc={providersIpcProp}
                value={selectedModelId}
                onChange={onSelectModel}
                onProviderChange={onModelProviderChange}
                providerValue={selectedProviderId}
                sessionModel={activeSession?.model ?? null}
                disabled={degraded}
              />
              <button
                type="button"
                className="send"
                data-testid="composer-send"
                aria-label="发送"
                title="发送"
                disabled={
                  !activeSessionId ||
                  degraded ||
                  sending ||
                  draft.trim().length === 0 ||
                  activeSession?.status === "paused" ||
                  (activeRequiresProvider && activeProviderState === "invalid") ||
                  (activeRequiresProvider &&
                    !activeSession?.provider_id &&
                    !reselectProviderId) ||
                  (activeRequiresProvider &&
                    activeSession?.provider_id !== undefined &&
                    activeModelState !== "active" &&
                    !reselectModelId) ||
                  (selectionRunInFlight &&
                    !activeSession?.provider_id &&
                    reselectProviderId !== null)
                }
                onClick={() => void onSend()}
              >
                <svg viewBox="0 0 24 24">
                  <path d="M5 12h14M13 6l6 6-6 6" />
                </svg>
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

          {/* M7-04：分屏第二窗格（UI-01 完整；EventStore 按会话隔离订阅）。 */}
          {splitSession ? (
            <SplitPane
              store={store}
              ipc={ipc}
              session={splitSession}
              historyLimit={historyLimit}
              streamItemHeight={streamItemHeight}
              streamHeight={streamHeight}
              degraded={degraded}
              onError={reportError}
              onRefreshSessions={refreshSessions}
            />
          ) : null}
        </div>
      </div>

      {/* ===== 右栏（原型 .right：文件 / 改动；折叠按钮浮于右上） ===== */}
      <aside
        className={rightOpen ? "right" : "right collapsed"}
        data-testid="right-panel"
        data-open={String(rightOpen)}
      >
        <div className="right-inner">
          <div className="r-head">
            <div className="r-tabs" role="tablist" aria-label="右栏分区">
              <button
                type="button"
                role="tab"
                aria-selected={rightTab === "files"}
                className={rightTab === "files" ? "r-tab on" : "r-tab"}
                data-testid="right-tab-files"
                onClick={() => setRightTab("files")}
              >
                <svg viewBox="0 0 24 24">
                  <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z" />
                </svg>
                文件
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={rightTab === "changes"}
                className={rightTab === "changes" ? "r-tab on" : "r-tab"}
                data-testid="right-tab-changes"
                onClick={() => setRightTab("changes")}
              >
                <svg viewBox="0 0 24 24">
                  <path d="M14 3H7a2 2 0 00-2 2v14a2 2 0 002 2h10a2 2 0 002-2V8z" />
                  <path d="M14 3v5h5" />
                </svg>
                改动
              </button>
            </div>
          </div>

          <div className="r-pane" hidden={rightTab !== "files"}>
            {/* M3-09 S-11：文件引用面板（只读；会话文件/项目文件；ADR-010 决策 1）。 */}
            <FilePanel
              ipc={artifactsIpcProp}
              sessionId={activeSessionId}
              workspaceRoot={activeSession?.workspace_root ?? null}
            />
          </div>

          <div className="r-pane" hidden={rightTab !== "changes"}>
            <div className="r-body">
              <p className="r-empty">改动视图（P1）</p>
            </div>
          </div>
        </div>
      </aside>

      {/* 原型 .panel-toggle：固定在右上角，折叠/展开右栏时位置不漂移。 */}
      <button
        type="button"
        className="panel-toggle"
        data-testid="right-panel-toggle"
        aria-expanded={rightOpen}
        aria-label={rightOpen ? "折叠右侧面板" : "展开右侧面板"}
        title={rightOpen ? "折叠右侧面板" : "展开右侧面板"}
        onClick={() => setRightOpen((open) => !open)}
      >
        <svg viewBox="0 0 24 24">
          <rect x="3" y="4" width="18" height="16" rx="2" />
          <path d="M15 4v16" />
        </svg>
      </button>
    </section>
  );
}
