/**
 * 多模态附件 IPC 与展示工具（M10-05/ADR-035 v1.1）。
 *
 * 口径：
 * - renderer 只做**轻量前置校验**（类型/大小/数量），权威校验在核心（魔数/像素/限额）；
 * - renderer 不接收原图 Buffer；缩略图经 `aether-attachment://thumb/<sha256>` 由
 *   Electron main + UtilityProcess 生成（ADR-035 D-035-3）；
 * - `attachment_add` 返回摘要不包含 `storage_path`（Q10 A）。
 */
import { client } from "@aether/protocol";

/** 单文件硬上限（ADR-035 D-035-1：10 MiB）。 */
export const ATTACHMENT_MAX_BYTES = 10 * 1024 * 1024;
/** 单条消息附件数上限（ADR-035 D-035-1：8）。 */
export const ATTACHMENT_MAX_PER_MESSAGE = 8;
/** 允许的 MIME 白名单（仅图像；SVG 明确拒绝）。 */
export const ATTACHMENT_MIME_WHITELIST = [
  "image/png",
  "image/jpeg",
  "image/webp",
  "image/gif",
] as const;
/** 文件选择器 accept（扩展名仅作 UI 提示；核心按魔数嗅探）。 */
export const ATTACHMENT_ACCEPT = ".png,.jpg,.jpeg,.webp,.gif,image/png,image/jpeg,image/webp,image/gif";
/** 缩略图协议（main 侧只允许 GET + 64hex + 路径白名单）。 */
export const ATTACHMENT_SCHEME = "aether-attachment";
export const MULTIMODAL_CAPABILITY = "multimodal";
export const MULTIMODAL_MODEL_UNVERIFIED = "multimodal_model_unverified";

/** `attachment_add/list` 返回的元数据摘要（不含 `storage_path`）。 */
export interface AttachmentSummary {
  attachment_id: string;
  sha256: string;
  mime: string;
  size_bytes: number;
  width?: number | null;
  height?: number | null;
  created_at: number;
}

/** 附件命令面（可注入替身；生产 = `@aether/protocol` typed client）。 */
export interface AttachmentIpc {
  add(sessionId: string, sourcePath: string): Promise<AttachmentSummary>;
  list(sessionId: string): Promise<AttachmentSummary[]>;
  remove(attachmentId: string): Promise<{ removed: boolean }>;
}

/** 生产实现。 */
export const attachmentIpc: AttachmentIpc = {
  async add(sessionId, sourcePath) {
    return client.attachmentAdd<AttachmentSummary>({
      session_id: sessionId,
      source_path: sourcePath,
      kind: "image",
    });
  },
  async list(sessionId) {
    const result = await client.attachmentList<{ items: AttachmentSummary[] }>({
      session_id: sessionId,
    });
    return result.items ?? [];
  },
  async remove(attachmentId) {
    return client.attachmentRemove<{ removed: boolean }>({
      attachment_id: attachmentId,
    });
  },
};

/** 缩略图 URL（renderer 只展示已生成/按需生成的 WebP）。 */
export function attachmentThumbUrl(sha256: string): string {
  return `${ATTACHMENT_SCHEME}://thumb/${sha256}`;
}

/** 运行时是否声明多模态能力（静态表与 hello 合并结果由 `runtimes_list` 提供）。 */
export function runtimeSupportsMultimodal(capabilities: string[] | undefined): boolean {
  return Boolean(capabilities?.includes(MULTIMODAL_CAPABILITY));
}

/**
 * 轻量前置校验（UI 即时反馈；权威判定仍在核心）。
 * 返回 `null` = 通过，否则返回稳定业务码（用于 `data-error-code`）。
 */
export function validateAttachmentCandidate(file: {
  size: number;
  type: string;
  name?: string;
}): "attachment_too_large" | "attachment_type_unsupported" | null {
  if (file.size > ATTACHMENT_MAX_BYTES) {
    return "attachment_too_large";
  }
  if (file.type && !(ATTACHMENT_MIME_WHITELIST as readonly string[]).includes(file.type)) {
    return "attachment_type_unsupported";
  }
  const name = file.name?.toLowerCase() ?? "";
  if (name.length > 0) {
    const extension = name.includes(".") ? name.slice(name.lastIndexOf(".")) : "";
    const allowed = [".png", ".jpg", ".jpeg", ".webp", ".gif"];
    if (!allowed.includes(extension)) {
      return "attachment_type_unsupported";
    }
  }
  return null;
}

/** 展示用文件名（去路径分隔符；与核心清洗同口径的轻量版本）。 */
export function attachmentDisplayName(path: string): string {
  const base = path.split(/[\\/]/).pop() ?? path;
  const cleaned = base.replace(/[\u0000-\u001f\u007f\u200e\u200f\u202a-\u202e\u2066-\u2069]/g, "").trim();
  return cleaned.length > 0 ? cleaned : "attachment";
}

/** 人类可读大小（诊断/附件条展示）。 */
export function formatAttachmentBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KiB`;
  return `${bytes} B`;
}
