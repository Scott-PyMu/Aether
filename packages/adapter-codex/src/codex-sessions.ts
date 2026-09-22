/**
 * Codex 原生会话别名映射（ADR-008 §3.3）。
 *
 * Codex 的 `thread_id` 由服务端在首个 run 生成、不可预生成；适配器对外暴露稳定别名
 * （`session.create` 的 `native_id`），并把 `别名 → thread_id` 原子持久化到状态目录，
 * 使核心仅凭 `sessions.config.native_id` 即可跨适配器进程恢复（Mode R）。
 */

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

interface SessionBinding {
  thread_id: string;
  updated_at: number;
}

interface StoreFile {
  v: 1;
  sessions: Record<string, SessionBinding>;
}

export class CodexSessionStore {
  private sessions = new Map<string, SessionBinding>();

  constructor(private readonly path: string) {}

  /** 从磁盘加载（文件缺失/损坏视为空映射；损坏时返回错误详情供诊断）。 */
  load(): string | null {
    let text: string;
    try {
      text = readFileSync(this.path, "utf8");
    } catch {
      return null;
    }
    try {
      const parsed = JSON.parse(text) as StoreFile;
      if (parsed.v !== 1 || typeof parsed.sessions !== "object" || parsed.sessions === null) {
        return "状态文件形状不符（v/sessions）";
      }
      for (const [alias, binding] of Object.entries(parsed.sessions)) {
        if (binding && typeof binding.thread_id === "string" && binding.thread_id.length > 0) {
          this.sessions.set(alias, {
            thread_id: binding.thread_id,
            updated_at: binding.updated_at ?? 0,
          });
        }
      }
      return null;
    } catch (error) {
      return error instanceof Error ? error.message : String(error);
    }
  }

  lookup(alias: string): string | null {
    return this.sessions.get(alias)?.thread_id ?? null;
  }

  bind(alias: string, threadId: string): void {
    this.sessions.set(alias, { thread_id: threadId, updated_at: Date.now() });
    this.save();
  }

  remove(alias: string): void {
    if (this.sessions.delete(alias)) this.save();
  }

  get size(): number {
    return this.sessions.size;
  }

  private save(): void {
    const payload: StoreFile = { v: 1, sessions: {} };
    for (const [alias, binding] of this.sessions) {
      payload.sessions[alias] = binding;
    }
    mkdirSync(dirname(this.path), { recursive: true });
    const tmp = `${this.path}.tmp`;
    writeFileSync(tmp, JSON.stringify(payload), "utf8");
    renameSync(tmp, this.path);
  }
}
