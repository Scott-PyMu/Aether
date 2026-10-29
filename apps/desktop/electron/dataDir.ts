/**
 * 数据目录解析（M9-07；设计 D3 / ADR-029 §3.5/§5.4）。
 *
 * 与 Rust `aether_app::startup::pointer` 保持同一优先级：
 * 1. `AETHER_DATA_DIR`（必须绝对路径；仍受 A4 检测约束）；
 * 2. 指针文件 `data-location.json`（默认 `data_local_dir()/Aether/`；可用
 *    `AETHER_DATA_LOCATION_FILE` 覆盖）；
 * 3. 缺省 `data_dir()/Aether`（Win `%APPDATA%\Aether`、macOS
 *    `~/Library/Application Support/Aether`、Linux `~/.local/share/Aether`）。
 *
 * main 只用于 S-12 展示、`shell.log` 落盘与离线诊断；**业务事实源仍是 daemon**
 * （daemon 在 `initialize` 响应中返回实际 `dataDir`，main 以该值为准）。
 */
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

/** 数据目录环境变量（与 Rust 同名）。 */
export const DATA_DIR_ENV = "AETHER_DATA_DIR";
/** 指针文件路径覆盖（与 Rust 同名）。 */
export const POINTER_ENV = "AETHER_DATA_LOCATION_FILE";
/** 应用目录名（D3）。 */
export const APP_DIR_NAME = "Aether";
/** 指针文件名。 */
export const POINTER_FILE_NAME = "data-location.json";
/** 指针文件版本（与 Rust `POINTER_VERSION` 一致）。 */
export const POINTER_VERSION = 1;
/** shell 日志目录名。 */
export const LOG_DIR_NAME = "logs";
/** shell 日志文件名（ADR-029 §4.6：main 脱敏后落盘、轮转）。 */
export const SHELL_LOG_FILE_NAME = "shell.log";
/** daemon 日志文件名（ADR-029 §3.5-3；daemon 写，main 只读快照）。 */
export const DAEMON_LOG_FILE_NAME = "aetherd.log";

export interface DataDirResolution {
  /** 解析结果（绝对路径；无法解析时为缺省候选）。 */
  dataDir: string;
  /** 解析来源。 */
  source: "env" | "pointer" | "default";
  /** 非致命解析问题（非法环境变量/指针损坏；S-12 展示用）。 */
  error?: string;
}

export interface ResolveOptions {
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  home?: string;
  /** 测试注入：指针文件读取（缺省 `readFileSync`）。 */
  readFile?: (file: string) => string;
  /** 测试注入：文件存在性（缺省 `existsSync`）。 */
  exists?: (file: string) => boolean;
}

/** 平台缺省 `data_dir()`。 */
export function defaultDataDir(
  platform: NodeJS.Platform,
  home: string,
  env: NodeJS.ProcessEnv = {},
): string {
  if (platform === "win32") {
    const appData = env.APPDATA ?? path.join(home, "AppData", "Roaming");
    return path.join(appData, APP_DIR_NAME);
  }
  if (platform === "darwin") {
    return path.join(home, "Library", "Application Support", APP_DIR_NAME);
  }
  const xdg = env.XDG_DATA_HOME;
  if (typeof xdg === "string" && xdg.length > 0 && path.isAbsolute(xdg)) {
    return path.join(xdg, APP_DIR_NAME);
  }
  return path.join(home, ".local", "share", APP_DIR_NAME);
}

/** 平台缺省 `data_local_dir()`（指针文件所在目录）。 */
export function defaultLocalDir(
  platform: NodeJS.Platform,
  home: string,
  env: NodeJS.ProcessEnv = {},
): string {
  if (platform === "win32") {
    const localAppData = env.LOCALAPPDATA ?? path.join(home, "AppData", "Local");
    return path.join(localAppData, APP_DIR_NAME);
  }
  if (platform === "darwin") {
    return path.join(home, "Library", "Application Support", APP_DIR_NAME);
  }
  const xdg = env.XDG_DATA_HOME;
  if (typeof xdg === "string" && xdg.length > 0 && path.isAbsolute(xdg)) {
    return path.join(xdg, APP_DIR_NAME);
  }
  return path.join(home, ".local", "share", APP_DIR_NAME);
}

/** 指针文件路径（环境变量覆盖优先）。 */
export function pointerFile(
  platform: NodeJS.Platform,
  home: string,
  env: NodeJS.ProcessEnv = {},
): string {
  const override = env[POINTER_ENV];
  if (typeof override === "string" && override.length > 0 && path.isAbsolute(override)) {
    return override;
  }
  return path.join(defaultLocalDir(platform, home, env), POINTER_FILE_NAME);
}

/** 解析数据目录；永不抛错（S-12 需要在任何启动状态下展示路径）。 */
export function resolveDataDir(options: ResolveOptions = {}): DataDirResolution {
  const env = options.env ?? process.env;
  const platform = options.platform ?? process.platform;
  const home = options.home ?? os.homedir();
  const readFile = options.readFile ?? ((file: string) => readFileSync(file, "utf8"));
  const exists = options.exists ?? existsSync;

  const fromEnv = env[DATA_DIR_ENV];
  if (typeof fromEnv === "string" && fromEnv.trim().length > 0) {
    if (path.isAbsolute(fromEnv)) {
      return { dataDir: path.normalize(fromEnv), source: "env" };
    }
    return {
      dataDir: defaultDataDir(platform, home, env),
      source: "default",
      error: `${DATA_DIR_ENV} 必须是绝对路径（当前值：${fromEnv}）`,
    };
  }

  const pointerPath = pointerFile(platform, home, env);
  if (exists(pointerPath)) {
    try {
      const raw = readFile(pointerPath);
      const parsed = JSON.parse(raw) as { v?: unknown; data_dir?: unknown };
      if (parsed.v !== POINTER_VERSION) {
        return {
          dataDir: defaultDataDir(platform, home, env),
          source: "default",
          error: `数据目录指针版本 ${String(parsed.v)} 不受支持（当前 ${POINTER_VERSION}）`,
        };
      }
      if (typeof parsed.data_dir !== "string" || !path.isAbsolute(parsed.data_dir)) {
        return {
          dataDir: defaultDataDir(platform, home, env),
          source: "default",
          error: "数据目录指针必须指向绝对路径",
        };
      }
      return { dataDir: path.normalize(parsed.data_dir), source: "pointer" };
    } catch (error) {
      return {
        dataDir: defaultDataDir(platform, home, env),
        source: "default",
        error: `读取数据目录指针失败：${error instanceof Error ? error.message : String(error)}`,
      };
    }
  }

  return { dataDir: defaultDataDir(platform, home, env), source: "default" };
}

/** `<data_dir>/logs`。 */
export function logDirFor(dataDir: string): string {
  return path.join(dataDir, LOG_DIR_NAME);
}

/** daemon 日志路径（与 Rust `log_path_for` 一致）。 */
export function daemonLogPathFor(dataDir: string): string {
  return path.join(logDirFor(dataDir), DAEMON_LOG_FILE_NAME);
}

/** shell 日志路径。 */
export function shellLogPathFor(dataDir: string): string {
  return path.join(logDirFor(dataDir), SHELL_LOG_FILE_NAME);
}
