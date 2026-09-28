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
  enabled: boolean;
  status: RuntimeStatus;
  status_reason?: string | null;
}

/** `session_list` / `session_create` 条目（核心 `Session` 实体投影）。 */
export interface SessionSummary {
  id: string;
  runtime_id: string;
  workspace_id?: string | null;
  parent_session_id?: string | null;
  title: string;
  status: SessionStatus;
  model?: string | null;
  created_at: number;
  updated_at: number;
  closed_at?: number | null;
}

/** `session_send` ack（快路径：消息与 run 行提交后返回，不等模型）。 */
export interface SessionSendResult {
  session_id: string;
  message_id: string;
  run_id: string;
  queued: boolean;
  duplicate: boolean;
}

/** `session_interrupt` 回执。 */
export interface SessionInterruptReport {
  session_id: string;
  interrupted_run?: string | null;
  cancelled_waiting_run?: string | null;
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
  runtime_id: string;
  title: string;
  workspace_id?: string;
  model?: string;
}

export interface SessionListFilter {
  runtime_id?: string;
  status?: SessionStatus;
  limit?: number;
}

export interface MessagesPageInput {
  session_id: string;
  last_seq?: number;
  limit?: number;
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
  createSession(input: SessionCreateInput): Promise<SessionSummary>;
  sendMessage(input: {
    session_id: string;
    text: string;
    client_msg_id: string;
  }): Promise<SessionSendResult>;
  interruptSession(sessionId: string): Promise<SessionInterruptReport>;
  disposeSession(sessionId: string): Promise<{ session_id: string; status: SessionStatus }>;
  messagesPage(input: MessagesPageInput): Promise<MessagesPageResult>;
  /** M3-06：仅终态（failed/timeout/cancelled）run 可重试（ADR-004）。 */
  retryRun(runId: string): Promise<RunRetryResult>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const sessionIpc: SessionIpc = {
  async listRuntimes() {
    return invoke<RuntimeInfo[]>("runtimes_list");
  },
  async listSessions(filter = {}) {
    return invoke<SessionSummary[]>("session_list", { payload: filter });
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
 * 运行时 `status_reason` 展示文案（M3-03；UI-UX §3.2/§5，D5 五种 + 握手超时）。
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
};
