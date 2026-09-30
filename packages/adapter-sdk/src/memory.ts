/**
 * 工作区记忆工具（M3-08；设计 D14「记忆：文件式记忆，无检索」）。
 *
 * 适配器侧实现（核心负责注入组合与 D9 权限判定；实际文件读写由本模块执行并经
 * 线协议上报 `tool.call_started/completed/failed`，权限经 `permission.request`
 * 回环，D9 边界不变）：
 *
 * - 优先级：`AGENTS.md` > `AETHER.md` > `CLAUDE.md`（与核心注入一致）；
 * - `memory.read` / `memory.append` / `memory.write` 三工具；
 * - 写入原子化（同目录临时文件 + fsync + rename）；写入中断不产生半写目标文件；
 * - 冲突处理（D14）：写入前记录目标文件 `mtime+size`，rename 前若被外部修改 →
 *   中止并返回 `memory_conflict`（不覆盖，提示重读）；
 * - 单次写入 1MB 上限（D9 记忆文件白名单）在权限回环判定（`content_bytes`）。
 */

import { promises as fs } from "node:fs";
import path from "node:path";

import { ulid } from "./ulid";

/** 记忆文件优先级（与核心 `MEMORY_FILE_PRIORITY` 一致）。 */
export const MEMORY_FILE_PRIORITY = ["AGENTS.md", "AETHER.md", "CLAUDE.md"] as const;

/** 注入上限（D14：32KB；核心侧截断 + 显式标记）。 */
export const MEMORY_INJECTION_MAX_BYTES = 32 * 1024;

/** 单次写入上限（D9 记忆白名单：1MB）。 */
export const MEMORY_FILE_MAX_BYTES = 1_048_576;

/** 冲突错误码（D14/UI-UX 错误表：`memory_conflict`）。 */
export const MEMORY_CONFLICT_CODE = "memory_conflict";
/** 读取失败错误码（工具失败诊断）。 */
export const MEMORY_READ_FAILED_CODE = "memory_read_failed";
/** 写入失败错误码（工具失败诊断）。 */
export const MEMORY_WRITE_FAILED_CODE = "memory_write_failed";

/** 文件快照（mtime + size；冲突判定唯一依据）。 */
export interface MemoryFileSnapshot {
  mtimeMs: number;
  size: number;
}

/** 记忆工具错误（`code` 为稳定错误码，映射 `tool.call_failed.error.code`）。 */
export class MemoryToolError extends Error {
  readonly code: string;
  readonly recoverable: boolean;

  constructor(code: string, message: string, recoverable = true) {
    super(message);
    this.name = "MemoryToolError";
    this.code = code;
    this.recoverable = recoverable;
  }
}

/** 原子写选项（故障注入：分块写与块间隔，供「写入中断」演练）。 */
export interface AtomicWriteOptions {
  /** 分块字节数（缺省一次性写）。 */
  chunkBytes?: number;
  /** 分块间隔毫秒（缺省 0）。 */
  chunkDelayMs?: number;
  /** 每块写入后回调（测试观测点）。 */
  onChunk?: (written: number, total: number) => void;
}

/** 优先级候选路径（不检查存在性）。 */
export function memoryFileCandidates(root: string): string[] {
  return MEMORY_FILE_PRIORITY.map((name) => path.join(root, name));
}

/** 选择工作区记忆文件（优先级顺序的第一个存在普通文件；无 → null）。 */
export async function selectMemoryFile(root: string): Promise<string | null> {
  for (const candidate of memoryFileCandidates(root)) {
    try {
      const stat = await fs.stat(candidate);
      if (stat.isFile()) return candidate;
    } catch {
      // 不存在 → 下一候选。
    }
  }
  return null;
}

/** 读取记忆文件（返回正文与快照；不存在/不可读 → `MemoryToolError`）。 */
export async function readMemoryFile(
  filePath: string,
): Promise<{ content: string; snapshot: MemoryFileSnapshot }> {
  try {
    const buffer = await fs.readFile(filePath);
    const stat = await fs.stat(filePath);
    return {
      content: buffer.toString("utf8"),
      snapshot: { mtimeMs: stat.mtimeMs, size: stat.size },
    };
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    throw new MemoryToolError(MEMORY_READ_FAILED_CODE, `记忆文件读取失败：${detail}`);
  }
}

/** 读取当前快照（不存在 → null）。 */
async function snapshotOrNull(filePath: string): Promise<MemoryFileSnapshot | null> {
  try {
    const stat = await fs.stat(filePath);
    if (!stat.isFile()) {
      throw new MemoryToolError(MEMORY_READ_FAILED_CODE, `目标不是普通文件：${filePath}`);
    }
    return { mtimeMs: stat.mtimeMs, size: stat.size };
  } catch (error) {
    if (error instanceof MemoryToolError) throw error;
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "ENOENT") return null;
    const detail = error instanceof Error ? error.message : String(error);
    throw new MemoryToolError(MEMORY_READ_FAILED_CODE, `记忆文件探测失败：${detail}`);
  }
}

/**
 * 原子写：同目录临时文件（`.name.aether-tmp-<ulid>`）分块写 + fsync + rename。
 *
 * 冲突判定（D14）：`expected` 为写入前记录的快照；rename 前重新 stat 目标，
 * 不一致（含「预期不存在但已被创建」）→ 删除临时文件并抛 `memory_conflict`。
 * 目标文件的半写状态不可能出现（内容只经 rename 原子进入目标路径）。
 */
export async function atomicWriteMemory(
  filePath: string,
  content: string,
  expected: MemoryFileSnapshot | null,
  options: AtomicWriteOptions = {},
): Promise<MemoryFileSnapshot> {
  const directory = path.dirname(filePath);
  const tempPath = path.join(directory, `.${path.basename(filePath)}.aether-tmp-${ulid()}`);
  const buffer = Buffer.from(content, "utf8");
  const chunkBytes = Math.max(1, options.chunkBytes ?? (buffer.length || 1));
  const chunkDelayMs = options.chunkDelayMs ?? 0;
  let handle: Awaited<ReturnType<typeof fs.open>> | undefined;
  try {
    handle = await fs.open(tempPath, "w");
    let written = 0;
    while (written < buffer.length) {
      const end = Math.min(buffer.length, written + chunkBytes);
      await handle.write(buffer.subarray(written, end));
      written = end;
      options.onChunk?.(written, buffer.length);
      if (chunkDelayMs > 0 && written < buffer.length) {
        await new Promise((resolve) => setTimeout(resolve, chunkDelayMs));
      }
    }
    await handle.sync();
    await handle.close();
    handle = undefined;

    // 冲突判定：rename 前复核目标状态（D14）。
    const current = await snapshotOrNull(filePath);
    if (!snapshotsEqual(current, expected)) {
      await fs.rm(tempPath, { force: true }).catch(() => {});
      throw new MemoryToolError(
        MEMORY_CONFLICT_CODE,
        "记忆文件已被外部修改，本次写入已中止（未覆盖原文件）；请重新读取后重试",
      );
    }
    await fs.rename(tempPath, filePath);
    const stat = await fs.stat(filePath);
    return { mtimeMs: stat.mtimeMs, size: stat.size };
  } catch (error) {
    if (handle) await handle.close().catch(() => {});
    await fs.rm(tempPath, { force: true }).catch(() => {});
    if (error instanceof MemoryToolError) throw error;
    const detail = error instanceof Error ? error.message : String(error);
    throw new MemoryToolError(MEMORY_WRITE_FAILED_CODE, `记忆文件原子写失败：${detail}`);
  }
}

/** 快照相等（容差 0：mtimeMs 与 size 都一致才视为未变）。 */
function snapshotsEqual(
  current: MemoryFileSnapshot | null,
  expected: MemoryFileSnapshot | null,
): boolean {
  if (current === null || expected === null) return current === expected;
  return current.mtimeMs === expected.mtimeMs && current.size === expected.size;
}

/** `memory.write`：覆盖写（先记录快照 → 原子写；不存在时按「新建」记录 null）。 */
export async function writeMemory(
  filePath: string,
  content: string,
  options: AtomicWriteOptions = {},
): Promise<MemoryFileSnapshot> {
  if (Buffer.byteLength(content, "utf8") > MEMORY_FILE_MAX_BYTES) {
    throw new MemoryToolError(
      MEMORY_WRITE_FAILED_CODE,
      `写入超过 1MB 上限（${Buffer.byteLength(content, "utf8")} > ${MEMORY_FILE_MAX_BYTES}）`,
    );
  }
  const expected = await snapshotOrNull(filePath);
  return atomicWriteMemory(filePath, content, expected, options);
}

/** `memory.append`：追加写（读现值 → 拼接 → 原子写覆盖）。 */
export async function appendMemory(
  filePath: string,
  text: string,
  options: AtomicWriteOptions = {},
): Promise<MemoryFileSnapshot> {
  const existing = await snapshotOrNull(filePath);
  const current = existing === null ? "" : (await readMemoryFile(filePath)).content;
  return writeMemory(filePath, `${current}${text}`, options);
}

/** `memory.read`：读取正文（工具结果不进入 `tool.call_*` payload，仅作执行侧校验）。 */
export async function readMemory(filePath: string): Promise<string> {
  return (await readMemoryFile(filePath)).content;
}
