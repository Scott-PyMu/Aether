/**
 * 审计查询/导出 IPC 契约（M6-03；设计 D9/SE-03、ADR-023、ADR-018）。
 *
 * 与 Rust 侧 `archive_control.rs` 的 JSON 形状一致（snake_case）：
 * - `audit_query` → `{ rows, total, limit, offset }`（`ts DESC, id DESC` 稳定排序）；
 * - `audit_export` → `{ export: ArchiveReport }`（kind=`audit` 的 JSONL + manifest）；
 *   外部目录复用 `startup_pick_target` 选择器（不新增命令）。
 *
 * 本文件是审计命令面的类型化封装（可注入替身；生产 = Tauri invoke）。
 */
import { invoke } from "@tauri-apps/api/core";

import type { ArchiveReport } from "./archive";

/** 审计行（与 `audit_log` 表列一一对应；`detail` 为脱敏后文本）。 */
export interface AuditRow {
  id: string;
  session_id: string | null;
  runtime_id: string | null;
  /** `user` / `agent` / `system`。 */
  actor: string;
  action: string;
  resource: string | null;
  detail: string | null;
  result: string | null;
  ts: number;
}

/** 审计过滤条件（等值 + 闭区间时间；与 `audit_query`/`audit_export` 同名口径）。 */
export interface AuditFilters {
  sessionId?: string;
  actor?: string;
  action?: string;
  resource?: string;
  fromTs?: number;
  toTs?: number;
}

export interface AuditQueryResponse {
  rows: AuditRow[];
  limit: number;
  offset: number;
  /** 是否还有下一页（v0.3 分页设计修订：不提供命中总数）。 */
  has_more: boolean;
}

export interface AuditExportResponse {
  export: ArchiveReport;
}

export interface AuditIpc {
  /** 审计查询（过滤 + 分页；`limit` ≤500 缺省 100）。 */
  query(filters: AuditFilters, page?: { limit?: number; offset?: number }): Promise<AuditQueryResponse>;
  /** 审计导出（缺省 `archives/`；`targetDir` = 外部目录）。 */
  exportAudit(filters: AuditFilters, targetDir?: string | null): Promise<AuditExportResponse>;
  /** 系统目录选择器（复用 `startup_pick_target`）；`null` = 取消。 */
  pickTargetDir(): Promise<string | null>;
}

/** 过滤条件 → 命令载荷（缺省字段省略；不发送 `null`/`undefined` 成员）。 */
export function filtersPayload(filters: AuditFilters): Record<string, unknown> {
  const payload: Record<string, unknown> = {};
  if (filters.sessionId) payload.session_id = filters.sessionId;
  if (filters.actor) payload.actor = filters.actor;
  if (filters.action) payload.action = filters.action;
  if (filters.resource) payload.resource = filters.resource;
  if (filters.fromTs !== undefined) payload.from_ts = filters.fromTs;
  if (filters.toTs !== undefined) payload.to_ts = filters.toTs;
  return payload;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const auditIpc: AuditIpc = {
  async query(filters, page) {
    return invoke<AuditQueryResponse>("audit_query", {
      payload: {
        ...filtersPayload(filters),
        ...(page?.limit !== undefined ? { limit: page.limit } : {}),
        ...(page?.offset !== undefined ? { offset: page.offset } : {}),
      },
    });
  },
  async exportAudit(filters, targetDir) {
    return invoke<AuditExportResponse>("audit_export", {
      payload: {
        ...filtersPayload(filters),
        ...(targetDir ? { target_dir: targetDir } : {}),
      },
    });
  },
  async pickTargetDir() {
    const result = await invoke<{ target_dir: string | null }>("startup_pick_target");
    return result.target_dir ?? null;
  },
};
