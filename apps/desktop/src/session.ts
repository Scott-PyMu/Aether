/**
 * 会话工作台 IPC 契约（M3-02；设计 D7、UI-01/02/05）。
 *
 * 与 Rust 侧 `session_backend.rs` 的 JSON 形状一致（snake_case）；T14 生成绑定
 * （`packages/protocol/src/bindings.ts`）经 `payload` 透传，本文件是工作台使用的
 * 类型化封装（命令名与 DTO 字段与生成物一致）。
 */
import { invoke } from "@tauri-apps/api/core";
import type { AetherEvent } from "@aether/protocol";

/** 会话状态（与附录 C `sessions.status` CHECK 枚举一一对应）。 */
export type SessionStatus =
  | "creating"
  | "idle"
  | "running"
  | "paused"
  | "waiting_permission"
  | "completed"
  | "failed"
  | "cancelled";

/** M6-01/ADR-014 决策 3：会话级权限模式（三档；与运行时同名同序）。 */
export type PermissionMode = "read_only" | "workspace_write" | "danger_full_access";

/** 权限模式缺省值（ADR-014 附录 B：`workspace_write`）。 */
export const PERMISSION_MODE_DEFAULT: PermissionMode = "workspace_write";

/** 权限模式取值判定（非法/缺失返回 `null`；不抛错）。 */
export function asPermissionMode(value: unknown): PermissionMode | null {
  return value === "read_only" || value === "workspace_write" || value === "danger_full_access"
    ? value
    : null;
}

/** 权限模式回显归一（非法值回退缺省 `workspace_write`；ADR-014 决策 3）。 */
export function normalizePermissionMode(value: unknown): PermissionMode {
  return asPermissionMode(value) ?? PERMISSION_MODE_DEFAULT;
}

/** 运行时状态（D5 监督状态机）。 */
export type RuntimeStatus =
  | "cold"
  | "starting"
  | "ready"
  | "degraded"
  | "disabled";

/** `runtimes_list` 条目（含 hello 上报的能力清单，UI-02 能力徽标）。 */
export interface RuntimeInfo {
  id: string;
  name: string;
  kind: string;
  version: string;
  protocol: string;
  capabilities: string[];
  /** M7-01/ADR-015：用户启停偏好（`runtimes.enabled` 持久化值；非静态字段）。 */
  enabled: boolean;
  status: RuntimeStatus;
  status_reason?: string | null;
  /** M7-01/ADR-015：在途会话/run 计数（禁用二次确认 `data-count` 依据；0 为合法值）。 */
  active_sessions?: number;
}

/** 思考深度档位缺省值（ADR-010 决策 2：默认「高」= 2）。 */
export const THINKING_DEPTH_DEFAULT = 2;
/** 思考深度档位范围（0–4）。 */
export const THINKING_DEPTH_MIN = 0;
export const THINKING_DEPTH_MAX = 4;
/** 思考深度能力项（D6/ADR-010：`runtime.capabilities` 字符串项，存在即支持）。 */
export const THINKING_DEPTH_CAPABILITY = "thinking_depth";
/** 暂停/恢复能力项（D6/ADR-017：`runtime.capabilities` 字符串项，存在即支持；
 * 未声明或连接未就绪 → 置灰 + `pause_unsupported`）。 */
export const PAUSE_RESUME_CAPABILITY = "pause_resume";
/** 暂停能力置灰 tooltip（UI-UX §5 文案；`pause_unsupported` 行）。 */
export const PAUSE_UNSUPPORTED_HINT = "当前运行时不支持暂停/恢复（需原生会话恢复能力）";
/** 暂停/恢复语义常驻声明（UI-UX §2.4；ADR-017 附录 A-8）。 */
export const PAUSE_SEMANTICS_NOTICE =
  "暂停 = 中断当前回复并冻结会话；恢复 = 重放被中断的输入（原生会话上下文保留，Mode R）；暂停不承诺保留未完成的生成内容";
/** `paused` 状态行文案（UI-UX §5；ADR-017 附录 A-8）。 */
export const PAUSED_STATUS_HINT =
  "已暂停：当前回复已中断；恢复后将重放该输入（原生上下文保留）";
/** 档位显示文案（ADR-010 与原型一致：关闭/低/高/极高/最大）。 */
export const THINKING_DEPTH_LABELS: Record<number, string> = {
  0: "关闭",
  1: "低",
  2: "高",
  3: "极高",
  4: "最大",
};
/** 能力置灰 tooltip（UI-UX §2.4/§5 文案）。 */
export const THINKING_DEPTH_UNSUPPORTED_HINT = "当前运行时不支持思考深度";
/** 警告码：运行时未声明能力、按缺省 2 应用（ADR-006 附录 B「警告码」子表首项）。 */
export const THINKING_DEPTH_UNSUPPORTED_CODE = "thinking_depth_unsupported";

/**
 * 非阻断警告条目（ADR-010 决策 4：`warnings[].code` 独立命名空间；
 * 首项 `thinking_depth_unsupported`，同步判定路径随响应返回）。
 */
export interface SessionWarning {
  code: string;
  field: string;
  runtime_id: string;
  message: string;
}

/**
 * `session_list` / `session_create` 条目（ADR-010 附录 B.4 的 `SessionSummary` DTO；
 * M3-09 引入，含可选 `workspace_root`；M3-10 追加 `thinking_depth` 与可选警告）。
 */
export interface SessionSummary {
  id: string;
  runtime_id: string;
  workspace_id?: string | null;
  parent_session_id?: string | null;
  title: string;
  status: SessionStatus;
  model?: string | null;
  /** M3-10：会话级思考深度（0–4；`session_create` 生效值回显）。 */
  thinking_depth?: number | null;
  /** M6-01/ADR-014：会话级权限模式（缺省 `workspace_write`；`session_list` 回显）。 */
  permission_mode?: string | null;
  /** M7-09/ADR-026：预设生效来源（未选预设省略）。 */
  preset_id?: string | null;
  /** M7-09/ADR-026：预设名称回显（来源为自定义预设时缺失名称，`preset_id` 亦为空）。 */
  preset_name?: string | null;
  created_at: number;
  updated_at: number;
  closed_at?: number | null;
  /** M3-09：绑定工作区的 canonical 根路径（未绑定省略）；文件面板「项目文件」展示。 */
  workspace_root?: string | null;
  /** M3-10：`session_create` 同步能力门警告（无警告省略；非阻断）。 */
  warnings?: SessionWarning[];
}

/** 运行时是否声明思考深度能力（存在即支持；ADR-010）。 */
export function supportsThinkingDepth(runtime: RuntimeInfo | null | undefined): boolean {
  return runtime?.capabilities.includes(THINKING_DEPTH_CAPABILITY) ?? false;
}

/** 运行时是否声明暂停/恢复能力（存在即支持；ADR-017：未声明 → 置灰 + `pause_unsupported`）。 */
export function supportsPauseResume(runtime: RuntimeInfo | null | undefined): boolean {
  return runtime?.capabilities.includes(PAUSE_RESUME_CAPABILITY) ?? false;
}

/**
 * 闸门排队位置（M7-02/ADR-022 附录 A-5：`session_send` 响应可选 `queue`）。
 *
 * `position` 仅指**全局 FIFO** 内位置（从 1 计）；`limit` = 当前闸门上限
 * （`clamp(floor(free_ram_gb / 1.5), 2, 10)`）。超闸门排队时出现（`queued` 字段
 * 仍表示会话等待槽，二者语义正交）。
 */
export interface RunQueuePosition {
  position: number;
  limit: number;
}

/** `session_send` ack（快路径：消息与 run 行提交后返回，不等模型）。 */
export interface SessionSendResult {
  session_id: string;
  message_id: string;
  run_id: string;
  queued: boolean;
  duplicate: boolean;
  /** M7-02/ADR-022：超闸门进入全局 FIFO 时的排队位置（否则省略）。 */
  queue?: RunQueuePosition;
  /** M3-10：同步能力门警告（无警告省略；非阻断）。 */
  warnings?: SessionWarning[];
}

/** `session_interrupt` 回执。 */
export interface SessionInterruptReport {
  session_id: string;
  interrupted_run?: string | null;
  cancelled_waiting_run?: string | null;
}

/** `session_pause` 回执（M7-03/ADR-017 附录 A-1）。 */
export interface SessionPauseResult {
  session_id: string;
  status: "paused";
  /** 转移前状态（idle / running / waiting_permission）。 */
  from: SessionStatus;
  interrupted_run?: string | null;
  waiting_kept?: string | null;
}

/** `session_resume` 回执（M7-03/ADR-017 附录 A-1）。 */
export interface SessionResumeResult {
  session_id: string;
  status: "running" | "idle";
  from: SessionStatus;
  /** 重放 run（仅 running 分支）。 */
  run_id?: string | null;
}

/** `messages_page` 中的消息行（`messages` 表投影；工作台消息基线）。 */
export interface MessageRow {
  id: string;
  session_id: string;
  run_id?: string | null;
  client_msg_id?: string | null;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  seq: number;
  created_at: number;
}

/** `run_retry` 回执（M3-06：一键重放产生的新 run；旧 run 保留审计）。 */
export interface RunRetryResult {
  session_id: string;
  run_id: string;
  input_message_id: string;
  queued: boolean;
}

/** `messages_page` 响应（事件补读 + 最近一页消息历史）。 */
export interface MessagesPageResult {
  session_id: string;
  last_seq?: number | null;
  max_seq: number | null;
  events: AetherEvent[];
  /** 仅最近一页（`last_seq` 缺省）返回。 */
  messages?: MessageRow[];
  complete: boolean;
}

export interface SessionCreateInput {
  /** M7-02/ADR-022：缺省 = 自动路由（固定优先级）；显式 = 手动指定（优先，不参与打分）。 */
  runtime_id?: string;
  title: string;
  workspace_id?: string;
  model?: string;
  /** M3-10/ADR-010：会话级思考深度（0–4；缺省 2）。 */
  thinking_depth?: number;
  /** M6-01/ADR-014 决策 3：会话级权限模式（缺省 `workspace_write`）。 */
  permission_mode?: PermissionMode;
  /** M7-09/ADR-026：Agent 预设（可选；合成提示词由核心完成；查无 → `preset_not_found`）。 */
  preset_id?: string;
}

export interface SessionListFilter {
  runtime_id?: string;
  status?: SessionStatus;
  limit?: number;
}

/** M7-07/ADR-025 §2.1-2：会话搜索入参（query ≤256；limit ≤100 缺省 20）。 */
export interface SessionSearchInput {
  query: string;
  limit?: number;
  offset?: number;
}

/** M7-07/ADR-025 §2.1-2：搜索命中（会话摘要 + snippet + 命中来源）。 */
export interface SessionSearchHit extends SessionSummary {
  snippet: string;
  source: "title" | "message";
}

/** M7-07/ADR-025 §2.1-3：改名回执。 */
export interface SessionRenameResult {
  session_id: string;
  title: string;
  updated_at: number;
}

/** M7-07/ADR-025 §2.1-4：硬删除回执。 */
export interface SessionDeleteResult {
  session_id: string;
  deleted: boolean;
}

export interface MessagesPageInput {
  session_id: string;
  last_seq?: number;
  limit?: number;
}

/** 随消息附带的文件引用（M7-06/ADR-024：D6 `session.send.references` 载荷形态）。 */
export interface SessionReference {
  path: string;
  kind: "file" | "directory";
}

/**
 * 工作台使用的会话命令面（可注入替身；生产 = Tauri invoke）。
 *
 * 生产监听/命令名与生成绑定一致（`runtimes_list` / `session_list` / `session_create` /
 * `session_send` / `session_interrupt` / `session_dispose` / `messages_page`）。
 */
export interface SessionIpc {
  listRuntimes(): Promise<RuntimeInfo[]>;
  listSessions(filter?: SessionListFilter): Promise<SessionSummary[]>;
  /** M7-07/ADR-025：会话全文检索（标题 + 消息正文）。 */
  searchSessions(input: SessionSearchInput): Promise<SessionSearchHit[]>;
  /** M7-07/ADR-025：会话改名（单写队列；`session.updated`）。 */
  renameSession(sessionId: string, title: string): Promise<SessionRenameResult>;
  /** M7-07/ADR-025：硬删除会话（不可恢复；在途/待审批 → `session_busy`）。 */
  deleteSession(sessionId: string): Promise<SessionDeleteResult>;
  createSession(input: SessionCreateInput): Promise<SessionSummary>;
  sendMessage(input: {
    session_id: string;
    text: string;
    client_msg_id: string;
    /** M3-10/ADR-010：本次 run 思考深度覆盖（0–4；缺省 = 会话级值）。 */
    thinking_depth?: number;
    /** M7-06/ADR-024：随消息附带的文件引用（≤32；引用 ≠ 预授权）。 */
    references?: SessionReference[];
  }): Promise<SessionSendResult>;
  interruptSession(sessionId: string): Promise<SessionInterruptReport>;
  /** M7-03/ADR-017：暂停会话（受控中断 + `paused`；`pause_resume` 能力门）。 */
  pauseSession(sessionId: string): Promise<SessionPauseResult>;
  /** M7-03/ADR-017：恢复会话（有暂停标记 → 重放 `paused → running`；否则 `idle`）。 */
  resumeSession(sessionId: string): Promise<SessionResumeResult>;
  disposeSession(sessionId: string): Promise<{ session_id: string; status: SessionStatus }>;
  messagesPage(input: MessagesPageInput): Promise<MessagesPageResult>;
  /** M3-06：仅终态（failed/timeout/cancelled）run 可重试（ADR-004）。 */
  retryRun(runId: string): Promise<RunRetryResult>;
  /** M6-01/ADR-014 决策 3：设置会话级权限模式（仅影响后续工具调用决策）。 */
  setPermissionMode(
    sessionId: string,
    mode: PermissionMode,
  ): Promise<{
    session_id: string;
    permission_mode: string;
    previous_permission_mode: string;
  }>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const sessionIpc: SessionIpc = {
  async listRuntimes() {
    return invoke<RuntimeInfo[]>("runtimes_list");
  },
  async listSessions(filter = {}) {
    return invoke<SessionSummary[]>("session_list", { payload: filter });
  },
  async searchSessions(input) {
    return invoke<SessionSearchHit[]>("session_search", { payload: input });
  },
  async renameSession(sessionId, title) {
    return invoke<SessionRenameResult>("session_rename", {
      payload: { session_id: sessionId, title },
    });
  },
  async deleteSession(sessionId) {
    return invoke<SessionDeleteResult>("session_delete", {
      payload: { session_id: sessionId },
    });
  },
  async createSession(input) {
    return invoke<SessionSummary>("session_create", { payload: input });
  },
  async sendMessage(input) {
    return invoke<SessionSendResult>("session_send", { payload: input });
  },
  async interruptSession(sessionId) {
    return invoke<SessionInterruptReport>("session_interrupt", {
      payload: { session_id: sessionId },
    });
  },
  async pauseSession(sessionId) {
    return invoke<SessionPauseResult>("session_pause", {
      payload: { session_id: sessionId },
    });
  },
  async resumeSession(sessionId) {
    return invoke<SessionResumeResult>("session_resume", {
      payload: { session_id: sessionId },
    });
  },
  async disposeSession(sessionId) {
    return invoke<{ session_id: string; status: SessionStatus }>("session_dispose", {
      payload: { session_id: sessionId },
    });
  },
  async messagesPage(input) {
    return invoke<MessagesPageResult>("messages_page", { payload: input });
  },
  async retryRun(runId) {
    return invoke<RunRetryResult>("run_retry", { payload: { run_id: runId } });
  },
  async setPermissionMode(sessionId, mode) {
    return invoke<{
      session_id: string;
      permission_mode: string;
      previous_permission_mode: string;
    }>("session_permission_mode_set", {
      payload: { session_id: sessionId, permission_mode: mode },
    });
  },
};

/** 会话状态展示文案。 */
export const SESSION_STATUS_LABELS: Record<SessionStatus, string> = {
  creating: "创建中",
  idle: "空闲",
  running: "运行中",
  paused: "已暂停",
  waiting_permission: "等待审批",
  completed: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

/** 运行时状态展示文案。 */
export const RUNTIME_STATUS_LABELS: Record<RuntimeStatus, string> = {
  cold: "未启动",
  starting: "启动中",
  ready: "就绪",
  degraded: "降级",
  disabled: "已禁用",
};

/**
 * 运行时 `status_reason` 展示文案（M3-03；UI-UX §3.2/§5，D5 五种 + 握手超时 +
 * ADR-015 `user_disabled`）。
 *
 * 未知 reason 原样展示（不隐藏诊断信息）。
 */
export const RUNTIME_REASON_LABELS: Record<string, string> = {
  start_failed: "启动失败",
  handshake_timeout: "握手超时",
  crash_loop: "崩溃循环",
  version_mismatch: "版本不匹配",
  untrusted: "未受信任（非官方清单）",
  storage_backpressure: "存储背压隔离",
  // ADR-015 附录 E-2/E-3：用户显式禁用（非故障；灰点语义由 RuntimePanel 承接）。
  user_disabled: "已由用户手动禁用（非故障）",
};
