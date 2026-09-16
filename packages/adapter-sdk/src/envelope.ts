/**
 * 事件信封 v1 构造（设计 D4，与 `aether-core::EventEnvelope` 字段一一对应）。
 *
 * 信封固定字段：`v/id/session_id/run_id/runtime_id/seq/ts/type/payload`。
 * `seq` 由会话内单一 sequencer 分配（M1-09 在 Mock 侧为单线程，天然单调）。
 */

import { ulid } from "./ulid";

export const EVENT_ENVELOPE_VERSION = 1;

export interface EventEnvelope {
  v: number;
  id: string;
  session_id: string;
  run_id: string | null;
  runtime_id: string;
  seq: number;
  ts: number;
  type: string;
  payload: Record<string, unknown>;
}

export interface EnvelopeContext {
  runtimeId: string;
  sessionId: string;
  runId?: string | null;
}

/** 会话内 seq 分配器（单调唯一）。 */
export class SessionSequencer {
  private seq = 0;

  constructor(private readonly sessionId: string) {}

  next(): number {
    this.seq += 1;
    return this.seq;
  }

  current(): number {
    return this.seq;
  }

  get id(): string {
    return this.sessionId;
  }
}

export function buildEnvelope(
  context: EnvelopeContext,
  type: string,
  payload: Record<string, unknown>,
  seq: number,
  options: { id?: string; ts?: number } = {},
): EventEnvelope {
  return {
    v: EVENT_ENVELOPE_VERSION,
    id: options.id ?? ulid(),
    session_id: context.sessionId,
    run_id: context.runId ?? null,
    runtime_id: context.runtimeId,
    seq,
    ts: options.ts ?? Date.now(),
    type,
    payload,
  };
}
