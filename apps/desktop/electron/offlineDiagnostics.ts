/**
 * S-12 离线导出诊断（M9-07 DoD-4/DoD-6；ADR-029 §3.5-8/§4.6/P2-9）。
 *
 * **不依赖 daemon**：S-12 出现时 daemon 已退出/不可用；main 直接收集
 * shell 日志、daemon stderr 尾、daemon 日志快照、版本/平台/路径，写入一个
 * JSON 诊断文件。`export_diagnostics`（daemon 命令）仍走 M3-05 的完整诊断包；
 * 本文件是壳级兜底，不替代业务诊断。
 *
 * 脱敏：所有自由文本经 `redactText`；环境变量经 `redactEnv`（密钥键剔除）。
 */
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

import type { ShellFatalState } from "./startup";
import { redactEnv, redactValue } from "./redact";

/** 离线诊断 schema 标识（稳定；M9-10 可扩展字段，不改变语义）。 */
export const SHELL_DIAGNOSTICS_SCHEMA = "aether.shell-diagnostics.v1";
/** 诊断文件默认前缀。 */
export const DIAGNOSTICS_FILE_PREFIX = "aether-diagnostics";
/** 日志尾部行数。 */
export const DIAGNOSTICS_LOG_LINES = 200;

export interface DiagnosticsLogs {
  shell?: string;
  daemon?: string;
  daemonStderr?: string;
}

export interface DiagnosticsVersions {
  app: string;
  electron: string;
  chrome: string;
  node: string;
  protocol: string;
  daemon?: string;
  core?: string;
}

export interface OfflineDiagnosticsInput {
  fatal: ShellFatalState;
  versions: DiagnosticsVersions;
  logs: DiagnosticsLogs;
  env?: NodeJS.ProcessEnv;
  now?: number;
  platform?: NodeJS.Platform;
  arch?: string;
}

/** 手动移动数据目录说明（UI-UX S-12；M9 不提供迁移按钮）。 */
export const MANUAL_DATA_DIR_INSTRUCTIONS = [
  "M9 不提供自动迁移按钮。若需更换数据目录：",
  "1. 完全退出 Aether（确认任务管理器/活动监视器中无 aetherd 进程）。",
  "2. 将整个数据目录（含 aether.db、logs/ 等）复制到新的本地非同步盘目录。",
  "3. 通过 AETHER_DATA_DIR 环境变量指向新目录后重启；或在应用配置目录写入 data-location.json 指针（{v:1,data_dir:\"<新路径>\"}）。",
  "4. 确认新目录可用后再删除旧目录；不要仅移动 aether.db 而遗漏 WAL/logs。",
  "5. 同步盘（OneDrive/iCloud/Dropbox 等）会被 A4 检测拒绝，请使用本地目录。",
].join("\n");

/** 构建离线诊断对象（纯函数；测试直接断言）。 */
export function buildOfflineDiagnostics(input: OfflineDiagnosticsInput): Record<string, unknown> {
  const now = input.now ?? Date.now();
  const env = input.env ?? process.env;
  return {
    schema: SHELL_DIAGNOSTICS_SCHEMA,
    generated_at: new Date(now).toISOString(),
    generated_at_ms: now,
    kind: "shell_fatal",
    platform: input.platform ?? process.platform,
    arch: input.arch ?? process.arch,
    versions: redactValue(input.versions),
    fatal: redactValue({
      reason: input.fatal.reason,
      message: input.fatal.message,
      phase: input.fatal.phase,
      attempts: input.fatal.attempts,
      log_path: input.fatal.logPath,
      data_dir: input.fatal.dataDir,
    }),
    paths: redactValue({
      data_dir: input.fatal.dataDir,
      shell_log: path.join(input.fatal.dataDir, "logs", "shell.log"),
      daemon_log: input.fatal.logPath,
    }),
    logs: redactValue({
      shell: input.logs.shell ?? "",
      daemon: input.logs.daemon ?? "",
      daemon_stderr: input.logs.daemonStderr ?? "",
    }),
    env: redactEnv(env),
    manual_data_dir: MANUAL_DATA_DIR_INSTRUCTIONS,
    note: "离线诊断由 Electron main 生成，不依赖 aetherd；密钥/令牌已脱敏。",
  };
}

/** 默认文件名（`aether-diagnostics-<ISO>.json`，冒号替换为 `-`）。 */
export function defaultDiagnosticsFileName(now = Date.now()): string {
  const stamp = new Date(now).toISOString().replace(/[:.]/g, "-");
  return `${DIAGNOSTICS_FILE_PREFIX}-${stamp}.json`;
}

/** 写入诊断文件；返回规范化路径。失败抛出（调用方展示 `data-result`）。 */
export function writeOfflineDiagnostics(
  targetPath: string,
  payload: Record<string, unknown>,
): string {
  if (typeof targetPath !== "string" || targetPath.trim().length === 0) {
    throw new Error("诊断导出路径为空");
  }
  if (!path.isAbsolute(targetPath)) {
    throw new Error("诊断导出路径必须是绝对路径");
  }
  const directory = path.dirname(targetPath);
  if (!existsSync(directory)) {
    throw new Error(`诊断导出目录不存在：${directory}`);
  }
  mkdirSync(directory, { recursive: true });
  writeFileSync(targetPath, `${JSON.stringify(payload, null, 2)}\n`, "utf8");
  return path.normalize(targetPath);
}
