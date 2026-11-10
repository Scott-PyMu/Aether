/**
 * 导入 IPC 契约（M10-04 / ADR-034；UI-UX S-06）。
 *
 * 与 Rust 侧 `aether-store::import::ImportJob` / `import_control.rs` 的 JSON 形状一致：
 * - `session_import` / `archive_import` → `{ job_id, status: "staging" }`（异步 job）；
 * - `import_status` → 作业状态/进度/汇总/警告（轮询 1s）；
 * - `import_cancel` → `{ status: "cancelled" }`（staging/validating/applying）。
 *
 * 真实导入在 `awaiting_restart` 后由应用重启完成原子切换；`dry_run` 只预览。
 */
import { client } from "@aether/protocol";

export type ImportMode = "remap" | "preserve";
export type ImportStatus =
  | "staging"
  | "validating"
  | "applying"
  | "awaiting_restart"
  | "completed"
  | "failed"
  | "cancelled";

export interface ImportProgress {
  rows: number;
  bytes: number;
  total_bytes?: number | null;
}

export interface ImportSummary {
  sessions: number;
  messages: number;
  events: number;
  runs: number;
  audits: number;
  duplicates: number;
  skipped: number;
}

export interface ImportWarning {
  code: string;
  path?: string | null;
  detail: string;
}

export interface ImportFailureInfo {
  code: string;
  message: string;
  details?: unknown;
}

export interface ImportJobStatus {
  job_id: string;
  kind: "session" | "archive";
  status: ImportStatus;
  mode: ImportMode;
  dry_run: boolean;
  source_path: string;
  source_bytes: number;
  progress: ImportProgress;
  summary?: ImportSummary | null;
  warnings?: ImportWarning[];
  warnings_truncated?: boolean;
  error?: ImportFailureInfo | null;
  created_at: number;
  updated_at: number;
}

export interface ImportStartResponse {
  job_id: string;
  status: "staging";
}

export interface ImportIpc {
  startSession(sourcePath: string, mode?: ImportMode, dryRun?: boolean): Promise<ImportStartResponse>;
  startArchive(sourcePath: string, mode?: ImportMode, dryRun?: boolean): Promise<ImportStartResponse>;
  status(jobId: string, includeWarnings?: boolean): Promise<ImportJobStatus>;
  cancel(jobId: string): Promise<{ status: string }>;
  /** 文件选择器（复用 `ref_pick`）；`null` = 取消。 */
  pickFile(): Promise<string | null>;
  /** 完成 `awaiting_restart` 导入：走应用重启（confirm:true）。 */
  restart(): Promise<void>;
}

/** 生产实现（typed client；命令入参为内层 DTO）。 */
export const importIpc: ImportIpc = {
  async startSession(sourcePath, mode = "remap", dryRun = false) {
    return client.sessionImport<ImportStartResponse>({
      source_path: sourcePath,
      mode,
      dry_run: dryRun,
    });
  },
  async startArchive(sourcePath, mode = "remap", dryRun = false) {
    return client.archiveImport<ImportStartResponse>({
      source_path: sourcePath,
      mode,
      dry_run: dryRun,
    });
  },
  async status(jobId, includeWarnings = true) {
    return client.importStatus<ImportJobStatus>({
      job_id: jobId,
      include_warnings: includeWarnings,
    });
  },
  async cancel(jobId) {
    return client.importCancel<{ status: string }>({ job_id: jobId });
  },
  async pickFile() {
    const result = await client.refPick<{ path: string | null }>({ kind: "file" });
    return result.path ?? null;
  },
  async restart() {
    await client.appRestart<void>({ confirm: true });
  },
};

/** 状态是否终态（不再轮询）。 */
export function importIsTerminal(status: ImportStatus): boolean {
  return (
    status === "completed" ||
    status === "failed" ||
    status === "cancelled" ||
    status === "awaiting_restart"
  );
}

/** 是否可取消（ADR-034 D-034-4：awaiting_restart 不可取消）。 */
export function importIsCancellable(status: ImportStatus): boolean {
  return status === "staging" || status === "validating" || status === "applying";
}
