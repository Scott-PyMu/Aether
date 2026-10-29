/**
 * 下载文件落盘工具（M9-10 DoD-4）。
 *
 * 下载先写入 `downloads/.quarantine/<id>.part`；用户确认后由本模块移动到
 * 最终路径。拆成独立模块是为了让「确认后才落到 Downloads」可单测（不依赖
 * Electron/Chromium 的下载管线）。
 */
import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import path from "node:path";

export interface DownloadFileOps {
  exists(file: string): boolean;
  mkdir(dir: string): void;
  rename(source: string, target: string): void;
  remove(file: string): void;
}

const nativeOps: DownloadFileOps = {
  exists: (file) => existsSync(file),
  mkdir: (dir) => mkdirSync(dir, { recursive: true }),
  rename: (source, target) => renameSync(source, target),
  remove: (file) => rmSync(file, { force: true }),
};

export interface PromoteResult {
  ok: boolean;
  error?: string;
}

/** 把隔离区文件移动到最终路径（同卷 rename；成功后源文件不保留）。 */
export function promoteDownloadedFile(
  quarantinePath: string,
  targetPath: string,
  ops: DownloadFileOps = nativeOps,
): PromoteResult {
  if (!ops.exists(quarantinePath)) {
    return { ok: false, error: "隔离区文件不存在（下载未完成或已被清理）" };
  }
  try {
    ops.mkdir(path.dirname(targetPath));
    ops.rename(quarantinePath, targetPath);
    return { ok: true };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

/** 删除隔离区文件（取消/失败清理；不触碰最终路径）。 */
export function discardDownloadedFile(
  quarantinePath: string,
  ops: DownloadFileOps = nativeOps,
): void {
  if (!ops.exists(quarantinePath)) {
    return;
  }
  try {
    ops.remove(quarantinePath);
  } catch {
    // 仍被占用时由下次启动清理；不阻塞取消。
  }
}
