/**
 * M10-05/ADR-035：`aether-attachment://` 协议 + UtilityProcess 缩略图池。
 *
 * 口径：
 * - 只处理 `GET aether-attachment://thumb/<64hex>`；
 * - 路径白名单仅 `<data_dir>/attachments/`，`sha256` 必须 64 位小写十六进制；
 * - renderer 只接收已生成/按需生成的 WebP 缩略图，**不接收原图 Buffer**；
 * - 解码在 Electron UtilityProcess（受限进程，非沙箱），并发 2、队列 16、
 *   单图 10s；崩溃重试 1 次后禁用附件并提示（ADR-035 D-035-4）。
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { protocol, utilityProcess, type UtilityProcess } from "electron";

export const ATTACHMENT_SCHEME = "aether-attachment";
const THUMB_HOST = "thumb";
const SHA_PATTERN = /^[0-9a-f]{64}$/;
const DECODE_TIMEOUT_MS = 10_000;
const MAX_QUEUE = 16;
const MAX_WORKERS = 2;

/** 诊断指标（不记绝对路径/内容）。 */
export const attachmentMetrics = {
  thumb_generated: 0,
  thumb_failed: 0,
  orphans_cleaned: 0,
};

export interface AttachmentProtocolOptions {
  /** 数据目录（启动迁移/恢复后可能变化，使用 getter 实时解析）。 */
  dataDir: () => string;
  log?: (event: string, detail?: Record<string, unknown>) => void;
}

interface DecodeJob {
  id: number;
  sourcePath: string;
  thumbPath: string;
  attempt: number;
  resolve: () => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout | null;
}

interface WorkerSlot {
  child: UtilityProcess;
  job: DecodeJob | null;
}

class AttachmentDecoderPool {
  private readonly slots: WorkerSlot[] = [];
  private readonly queue: DecodeJob[] = [];
  private nextId = 1;
  private disabled = false;

  constructor(private readonly log: AttachmentProtocolOptions["log"]) {}

  get isDisabled(): boolean {
    return this.disabled;
  }

  decode(sourcePath: string, thumbPath: string): Promise<void> {
    if (this.disabled) {
      return Promise.reject(new Error("attachment_decoder_disabled"));
    }
    if (this.queue.length >= MAX_QUEUE) {
      return Promise.reject(new Error("attachment_decoder_busy"));
    }
    return new Promise((resolve, reject) => {
      this.queue.push({
        id: this.nextId++,
        sourcePath,
        thumbPath,
        attempt: 0,
        resolve,
        reject,
        timer: null,
      });
      this.pump();
    });
  }

  dispose(): void {
    for (const job of this.queue.splice(0)) {
      if (job.timer) clearTimeout(job.timer);
      job.reject(new Error("attachment_decoder_disposed"));
    }
    for (const slot of this.slots.splice(0)) {
      slot.child.kill();
    }
  }

  private spawn(): WorkerSlot | null {
    try {
      const child = utilityProcess.fork(path.join(__dirname, "attachmentWorker.cjs"), [], {
        serviceName: "Aether Attachment Decoder",
      });
      const slot: WorkerSlot = { child, job: null };
      child.on("message", (message: unknown) => this.onMessage(slot, message));
      child.on("exit", () => this.onExit(slot));
      return slot;
    } catch (error) {
      this.log?.("attachment.decoder.spawn_failed", { error: String(error) });
      return null;
    }
  }

  private pump(): void {
    if (this.disabled) return;
    while (this.slots.length < MAX_WORKERS) {
      const slot = this.spawn();
      if (!slot) return;
      this.slots.push(slot);
    }
    for (const slot of this.slots) {
      if (slot.job || this.queue.length === 0) continue;
      const job = this.queue.shift();
      if (!job) break;
      slot.job = job;
      job.timer = setTimeout(() => this.onTimeout(slot, job), DECODE_TIMEOUT_MS);
      slot.child.postMessage({
        id: job.id,
        sourcePath: job.sourcePath,
        thumbPath: job.thumbPath,
      });
    }
  }

  private onMessage(slot: WorkerSlot, message: unknown): void {
    const data = (message as { data?: unknown })?.data ?? message;
    const payload = data as { id?: number; ok?: boolean; error?: string };
    const job = slot.job;
    if (!job || payload?.id !== job.id) return;
    if (job.timer) clearTimeout(job.timer);
    job.timer = null;
    slot.job = null;
    if (payload.ok) {
      attachmentMetrics.thumb_generated += 1;
      job.resolve();
    } else if (job.attempt < 1) {
      job.attempt += 1;
      this.queue.unshift(job);
    } else {
      attachmentMetrics.thumb_failed += 1;
      job.reject(new Error(payload.error ?? "attachment_thumb_failed"));
    }
    this.pump();
  }

  private onTimeout(slot: WorkerSlot, job: DecodeJob): void {
    if (slot.job !== job) return;
    slot.job = null;
    job.timer = null;
    // 杀进程重启；同一任务重试一次（10s 预算重置）。
    try {
      slot.child.kill();
    } catch {
      /* 已退出 */
    }
    if (job.attempt < 1) {
      job.attempt += 1;
      this.queue.unshift(job);
    } else {
      attachmentMetrics.thumb_failed += 1;
      job.reject(new Error("attachment_decode_timeout"));
    }
    this.pump();
  }

  private onExit(slot: WorkerSlot): void {
    const index = this.slots.indexOf(slot);
    if (index >= 0) this.slots.splice(index, 1);
    const job = slot.job;
    slot.job = null;
    if (job) {
      if (job.timer) clearTimeout(job.timer);
      job.timer = null;
      if (job.attempt < 1) {
        job.attempt += 1;
        this.queue.unshift(job);
      } else {
        // 崩溃重试后仍失败：禁用附件缩略图（ADR-035 D-035-4）。
        this.disabled = true;
        attachmentMetrics.thumb_failed += 1;
        job.reject(new Error("attachment_decoder_crashed"));
        this.log?.("attachment.decoder.disabled", {});
      }
    }
    this.pump();
  }
}

let pool: AttachmentDecoderPool | null = null;

/** 懒创建（避免 app ready 前启动 UtilityProcess）。 */
function decoderPool(log?: AttachmentProtocolOptions["log"]): AttachmentDecoderPool {
  if (pool === null) {
    pool = new AttachmentDecoderPool(log);
  }
  return pool;
}

/** 退出时回收（before-quit）。 */
export function disposeAttachmentProtocol(): void {
  pool?.dispose();
  pool = null;
}

/** 解析原图路径（仅 `<data_dir>/attachments/<prefix>/<sha>.<ext>`）。 */
function resolveSource(dataDir: string, sha256: string): string | null {
  const prefix = path.join(dataDir, "attachments", sha256.slice(0, 2));
  if (!existsSync(prefix)) return null;
  for (const entry of readdirSync(prefix)) {
    if (entry.startsWith(`${sha256}.`)) {
      const candidate = path.join(prefix, entry);
      const canonical = path.resolve(candidate);
      const root = path.resolve(dataDir, "attachments") + path.sep;
      if (!canonical.startsWith(root)) return null;
      try {
        return statSync(canonical).isFile() ? canonical : null;
      } catch {
        return null;
      }
    }
  }
  return null;
}

function thumbResponse(body: Buffer): Response {
  return new Response(new Uint8Array(body), {
    status: 200,
    headers: {
      "Content-Type": "image/webp",
      "Cache-Control": "private, max-age=86400",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

function errorResponse(status: number, message: string): Response {
  return new Response(message, {
    status,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

/** 注册 `aether-attachment://` 协议（app ready、窗口创建前调用）。 */
export function registerAttachmentProtocol(options: AttachmentProtocolOptions): void {
  const { dataDir, log } = options;
  protocol.handle(ATTACHMENT_SCHEME, async (request) => {
    if (request.method !== "GET") {
      return errorResponse(405, "Method Not Allowed");
    }
    let url: URL;
    try {
      url = new URL(request.url);
    } catch {
      return errorResponse(400, "Bad Request");
    }
    if (url.host !== THUMB_HOST) {
      return errorResponse(404, "Not Found");
    }
    const sha256 = url.pathname.replace(/^\//, "").toLowerCase();
    if (!SHA_PATTERN.test(sha256)) {
      return errorResponse(400, "Bad Request");
    }
    const data = dataDir();
    const thumbDir = path.join(data, "attachments", "thumbs");
    const thumbPath = path.join(thumbDir, `${sha256}.webp`);
    try {
      if (existsSync(thumbPath)) {
        return thumbResponse(readFileSync(thumbPath));
      }
      const source = resolveSource(data, sha256);
      if (!source) {
        return errorResponse(404, "Not Found");
      }
      mkdirSync(thumbDir, { recursive: true });
      const temp = `${thumbPath}.${process.pid}.tmp`;
      // 二次校验 source 目录仍在白名单内（resolveSource 已 canonicalize）。
      await decoderPool(log).decode(source, temp);
      renameSync(temp, thumbPath);
      return thumbResponse(readFileSync(thumbPath));
    } catch (error) {
      const message = String(error instanceof Error ? error.message : error);
      attachmentMetrics.thumb_failed += 1;
      log?.("attachment.thumb.failed", {
        sha_prefix: createHash("sha256").update(sha256).digest("hex").slice(0, 8),
        error: message,
      });
      if (message.includes("unsupported")) return errorResponse(415, "Unsupported Media Type");
      if (message.includes("too_large")) return errorResponse(413, "Payload Too Large");
      if (message.includes("disabled")) return errorResponse(503, "Decoder Unavailable");
      return errorResponse(500, "Internal Error");
    }
  });
}
