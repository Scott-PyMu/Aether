/**
 * DSH 带外 delta 通道（ADR-002 §3.6 / M2-11 DoD3/5）：JSONL sidecar 读取 + 前缀去重。
 *
 * 插件经 `AETHER_DSH_DELTA_FILE` 追加帧；适配器按增量偏移轮询读取，路由到 run 后：
 * - 插件 delta 作为 `message.delta` 上报（token 级）；
 * - ACP committed `agent_message_chunk` 到达时按**前缀消费**去重：以流式前缀为起点，
 *   仅上报 final-only 后缀；
 * - 前缀不一致（通道截断/丢帧/丢弃 end）→ 丢弃增量、以 committed 全量重建
 *   （`message.completed` 与 ACP final 一致）。
 */

import { closeSync, openSync, readSync, statSync } from "node:fs";

/** 带外帧（形状由插件写入；未知字段忽略）。 */
export interface DeltaFrame {
  v?: number;
  type: string;
  attemptId?: string;
  sessionId?: string;
  chunkType?: string;
  text?: string;
  contract?: string;
  dshVersion?: string | null;
  at?: number;
}

/** 解析一行（非法行返回 `null`，不中断读取）。 */
export function parseDeltaLine(line: string): DeltaFrame | null {
  try {
    const parsed = JSON.parse(line) as unknown;
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
    const frame = parsed as Record<string, unknown>;
    if (typeof frame["type"] !== "string") return null;
    const result: DeltaFrame = { type: frame["type"] };
    if (typeof frame["v"] === "number") result.v = frame["v"];
    if (typeof frame["attemptId"] === "string") result.attemptId = frame["attemptId"];
    if (typeof frame["sessionId"] === "string") result.sessionId = frame["sessionId"];
    if (typeof frame["chunkType"] === "string") result.chunkType = frame["chunkType"];
    if (typeof frame["text"] === "string") result.text = frame["text"];
    if (typeof frame["contract"] === "string") result.contract = frame["contract"];
    if (typeof frame["dshVersion"] === "string" || frame["dshVersion"] === null) {
      result.dshVersion = frame["dshVersion"] as string | null;
    }
    if (typeof frame["at"] === "number") result.at = frame["at"];
    return result;
  } catch {
    return null;
  }
}

export interface DeltaChannelOptions {
  /** 轮询间隔（默认 25ms）。 */
  pollMs?: number;
  onFrame: (frame: DeltaFrame) => void;
  onMalformedLine?: (line: string) => void;
}

/** sidecar 通道读取器（增量偏移；`stop()` 后不再读取）。 */
export class DeltaChannel {
  readonly path: string;
  private readonly options: Required<Pick<DeltaChannelOptions, "pollMs">> & DeltaChannelOptions;
  private timer: NodeJS.Timeout | null = null;
  private offset = 0;
  private buffered = "";
  private stopped = false;
  private framesRead = 0;

  constructor(path: string, options: DeltaChannelOptions) {
    this.path = path;
    this.options = { pollMs: 25, ...options };
  }

  get frameCount(): number {
    return this.framesRead;
  }

  get bytesRead(): number {
    return this.offset;
  }

  start(): void {
    if (this.timer !== null || this.stopped) return;
    this.timer = setInterval(() => this.poll(), this.options.pollMs);
    if (typeof this.timer.unref === "function") this.timer.unref();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** 立即读取一次（测试/收口前同步 flush）。 */
  poll(): DeltaFrame[] {
    const frames: DeltaFrame[] = [];
    let size: number;
    try {
      size = statSync(this.path).size;
    } catch {
      return frames;
    }
    if (size < this.offset) {
      // 文件被截断/替换（故障注入）→ 从 0 重读。
      this.offset = 0;
      this.buffered = "";
    }
    if (size === this.offset) return frames;
    const length = size - this.offset;
    const buffer = Buffer.alloc(length);
    let fd: number;
    try {
      fd = openSync(this.path, "r");
    } catch {
      return frames;
    }
    try {
      const read = readSync(fd, buffer, 0, length, this.offset);
      this.offset += read;
      this.buffered += buffer.subarray(0, read).toString("utf8");
    } finally {
      closeSync(fd);
    }
    let index = this.buffered.indexOf("\n");
    while (index >= 0) {
      const line = this.buffered.slice(0, index).replace(/\r$/, "");
      this.buffered = this.buffered.slice(index + 1);
      if (line.trim().length > 0) {
        const frame = parseDeltaLine(line);
        if (frame) {
          this.framesRead += 1;
          this.options.onFrame(frame);
          frames.push(frame);
        } else {
          this.options.onMalformedLine?.(line);
        }
      }
      index = this.buffered.indexOf("\n");
    }
    return frames;
  }
}

export interface ReconcileResult {
  /** 应作为 `message.delta` 上报的后缀（可为空）。 */
  suffix: string;
  /** 前缀不一致 → 已丢弃流式增量并按 committed 重建（DoD5 兜底）。 */
  fallback: boolean;
  /** 丢弃的增量字符数（回归指标）。 */
  droppedChars: number;
}

/**
 * 前缀去重调和器（单 run）。
 *
 * 用法：插件 delta → [`onChunk`]；ACP committed chunk → [`onCommitted`]；
 * run 终态时 [`finalText`] 即权威正文（ACP final）。
 */
export class DeltaReconciler {
  private streamed = "";
  private committed = "";
  private endSeen = false;
  private fallbackUsed = false;
  private dropped = 0;
  private chunks = 0;

  /** 已消费的流式文本（= 已上报 delta 拼接）。 */
  get streamedText(): string {
    return this.streamed;
  }

  /** ACP committed 全文（终稿权威）。 */
  get committedText(): string | undefined {
    return this.committed.length > 0 ? this.committed : undefined;
  }

  get endFrameSeen(): boolean {
    return this.endSeen;
  }

  get fallbackTriggered(): boolean {
    return this.fallbackUsed;
  }

  get droppedChars(): number {
    return this.dropped;
  }

  get chunkCount(): number {
    return this.chunks;
  }

  /** 插件 delta：返回应上报的后缀（去重后）。 */
  onChunk(text: string): string {
    if (text.length === 0) return "";
    this.chunks += 1;
    if (this.committed.length > 0) {
      // committed 已到，迟到的插件帧不再上报（避免重复）。
      this.dropped += text.length;
      return "";
    }
    this.streamed += text;
    return text;
  }

  /** ACP committed chunk 到达：前缀消费 + 兜底重建。 */
  onCommitted(text: string): ReconcileResult {
    if (text.startsWith(this.streamed)) {
      // 正常前进 / final-only 后缀：只补未流式到达的部分。
      const suffix = text.slice(this.streamed.length);
      this.committed = text;
      this.streamed = text;
      return { suffix, fallback: false, droppedChars: this.dropped };
    }
    if (this.streamed.startsWith(text)) {
      // committed 落后于已流式内容（渐进 committed）→ 记录，不重发、不兜底。
      this.committed = text;
      return { suffix: "", fallback: false, droppedChars: this.dropped };
    }
    // 前缀不一致（丢帧/截断/乱序导致的不完整增量）→ 丢弃流式，按 committed 重建。
    this.fallbackUsed = true;
    this.dropped += this.streamed.length;
    this.committed = text;
    this.streamed = text;
    return { suffix: "", fallback: true, droppedChars: this.dropped };
  }

  markEnd(): void {
    this.endSeen = true;
  }

  /**
   * 终稿：ACP committed 优先；committed 落后于已流式内容时取流式拼接
   * （保证 ≥ 已上报增量；normal 路径两者恒等）。
   */
  finalText(): string {
    return this.committed.length >= this.streamed.length ? this.committed : this.streamed;
  }
}
