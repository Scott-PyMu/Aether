/**
 * 4 条壳本地命令（M9-07；ADR-029 §3.4/§4.6/§5.3）。
 *
 * `app_exit` / `app_restart` / `ref_pick` / `startup_pick_target` 由 **Electron main
 * 直接处理**，不进入 daemon（59/4 路由口径）。严格解析与 Rust
 * `parse_strict`/DTO 校验一致：未知字段、类型、枚举、缺失字段全部在壳层拒绝，
 * 不触达后端、不落库。
 *
 * 路径：对话框返回值先 `realpath`（canonicalize）再按 kind 校验存在性；
 * 返回 canonical 路径字符串。后续 `artifact_add`/`startup_migrate` 仍由 daemon
 * 复核（双端校验，daemon 为最终权威）。
 */
import { realpathSync, statSync } from "node:fs";

import { RPC_CODE } from "./daemonClient";

/** 壳本地命令名（与 `aether_app::SHELL_COMMANDS` 一致）。 */
export const SHELL_COMMANDS = [
  "app_exit",
  "app_restart",
  "ref_pick",
  "startup_pick_target",
] as const;

export type ShellCommand = (typeof SHELL_COMMANDS)[number];

/** `ref_pick` 选择类型（与 Rust `RefPickKind` 一致）。 */
export type RefPickKind = "file" | "directory";

/** 壳层命令错误（形状与 daemon JSON-RPC error 对齐）。 */
export class ShellLocalError extends Error {
  readonly code: number;
  readonly aetherCode: string;
  readonly field?: string;
  readonly details?: Record<string, unknown>;

  constructor(
    code: number,
    aetherCode: string,
    message: string,
    options: { field?: string; details?: Record<string, unknown> } = {},
  ) {
    super(message);
    this.name = "ShellLocalError";
    this.code = code;
    this.aetherCode = aetherCode;
    this.field = options.field;
    this.details = options.details;
  }

  toWire(): { code: number; message: string; data: Record<string, unknown> } {
    return {
      code: this.code,
      message: this.message,
      data: {
        aether_code: this.aetherCode,
        ...(this.field ? { field: this.field } : {}),
        ...(this.details ? { details: this.details } : {}),
      },
    };
  }
}

/** 壳本地命令执行结果。 */
export interface ShellLocalOutcome {
  result: unknown;
  /** 响应发出后执行（退出/重启）；失败不影响响应。 */
  after?: () => void;
}

/** 注入点（生产 = Electron dialog/app；测试替身）。 */
export interface ShellLocalDeps {
  /** 打开系统选择器；取消返回 `null`。 */
  pick(kind: RefPickKind): Promise<string | null>;
  /** 请求退出（走关闭序列；M9-09 预算）。 */
  requestExit(code: number): void;
  /** 请求重启（关闭序列后 relaunch）。 */
  requestRestart(): void;
}

/** 是否壳本地命令。 */
export function isShellLocalCommand(method: string): method is ShellCommand {
  return (SHELL_COMMANDS as readonly string[]).includes(method);
}

/**
 * 执行壳本地命令。
 *
 * 无参命令（`startup_pick_target`）：`params` 省略/`null`/`{}` 等价；
 * 其余命令只接受精确 DTO（`deny_unknown_fields` 语义）。
 */
export async function handleShellLocalCommand(
  method: string,
  params: unknown,
  deps: ShellLocalDeps,
): Promise<ShellLocalOutcome> {
  switch (method) {
    case "app_exit": {
      parseConfirm("app_exit", params);
      return {
        result: { exiting: true },
        after: () => deps.requestExit(0),
      };
    }
    case "app_restart": {
      parseConfirm("app_restart", params);
      return {
        result: { restarting: true },
        after: () => deps.requestRestart(),
      };
    }
    case "ref_pick": {
      const kind = parseRefPickKind(params);
      const picked = await deps.pick(kind);
      const path = picked === null ? null : validatePickedPath(picked, kind);
      return { result: { path } };
    }
    case "startup_pick_target": {
      parseNoParams("startup_pick_target", params);
      const picked = await deps.pick("directory");
      const target = picked === null ? null : validatePickedPath(picked, "directory");
      return { result: { target_dir: target } };
    }
    default:
      throw new ShellLocalError(
        RPC_CODE.methodNotFound,
        "not_implemented",
        `未知命令：${method}`,
      );
  }
}

/** 校验并 canonicalize 对话框返回值。 */
export function validatePickedPath(raw: string, kind: RefPickKind): string {
  if (typeof raw !== "string" || raw.length === 0) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "path_rejected",
      "选择器返回空路径",
      { field: "path" },
    );
  }
  let canonical: string;
  try {
    canonical = realpathSync.native(raw);
  } catch (error) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "path_rejected",
      `路径不可用：${error instanceof Error ? error.message : String(error)}`,
      { field: "path" },
    );
  }
  let stats;
  try {
    stats = statSync(canonical);
  } catch (error) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "path_rejected",
      `路径不可用：${error instanceof Error ? error.message : String(error)}`,
      { field: "path" },
    );
  }
  if (kind === "file" && !stats.isFile()) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "path_rejected",
      "所选路径不是文件",
      { field: "path" },
    );
  }
  if (kind === "directory" && !stats.isDirectory()) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "path_rejected",
      "所选路径不是目录",
      { field: "path" },
    );
  }
  return canonical;
}

function parseConfirm(method: "app_exit" | "app_restart", params: unknown): void {
  const object = parseExactObject(method, params, ["confirm"]);
  if (!("confirm" in object)) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "missing_field",
      "缺少字段：confirm",
      { field: "confirm" },
    );
  }
  if (typeof object.confirm !== "boolean") {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_type",
      "confirm 必须是布尔值",
      { field: "confirm" },
    );
  }
  if (!object.confirm) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_value",
      "必须显式 confirm:true（重启/退出将中断全部在途 run）",
      { field: "confirm" },
    );
  }
}

function parseRefPickKind(params: unknown): RefPickKind {
  const object = parseExactObject("ref_pick", params, ["kind"]);
  if (!("kind" in object)) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "missing_field",
      "缺少字段：kind",
      { field: "kind" },
    );
  }
  if (typeof object.kind !== "string") {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_type",
      "kind 必须是字符串",
      { field: "kind" },
    );
  }
  if (object.kind !== "file" && object.kind !== "directory") {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_enum",
      "kind 必须是 file 或 directory",
      { field: "kind" },
    );
  }
  return object.kind;
}

function parseNoParams(method: string, params: unknown): void {
  if (params === undefined || params === null) {
    return;
  }
  if (typeof params !== "object" || Array.isArray(params)) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_type",
      `${method} 不接受参数`,
    );
  }
  const keys = Object.keys(params as Record<string, unknown>);
  if (keys.length > 0) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "unknown_field",
      `不接受字段：${keys[0]}`,
      { field: keys[0] },
    );
  }
}

function parseExactObject(
  method: string,
  params: unknown,
  allowed: readonly string[],
): Record<string, unknown> {
  if (typeof params !== "object" || params === null || Array.isArray(params)) {
    throw new ShellLocalError(
      RPC_CODE.invalidParams,
      "invalid_type",
      `${method} 参数必须是对象`,
    );
  }
  const object = params as Record<string, unknown>;
  for (const key of Object.keys(object)) {
    if (!allowed.includes(key)) {
      throw new ShellLocalError(
        RPC_CODE.invalidParams,
        "unknown_field",
        `未知字段：${key}`,
        { field: key },
      );
    }
  }
  return object;
}
