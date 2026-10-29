/**
 * Electron 壳本地命令（M9-06 DoD-3；ADR-029 §4.6 / 设计 D7）。
 *
 * 4 条壳本地命令中的两条选择器由 **Electron main 直接处理**，不进入 `aetherd`：
 * - `ref_pick`（ADR-010）：`{ kind: "file" | "directory" }` → `{ path: string | null }`；
 * - `startup_pick_target`（ADR-006）：无参数 → `{ target_dir: string | null }`；
 *   **仍是通用目录选择器**（备份/导出/审计/归档/工作区复用）；M9 仅不用于启动迁移。
 *
 * 口径：
 * - 参数严格解析（与 `aether-app` 的 `deny_unknown_fields` + 枚举白名单等价）；
 * - 返回 **canonical 路径**（`fs.realpathSync.native`，解析软链接/Junction/8.3 短名），
 *   取消 → `null`；
 * - 路径仍由 daemon 在后续 `artifact_add` / `backup_*` / `startup_migrate` 复核
 *   （选择器不做默认目录信任）；
 * - 本模块**不依赖 Electron**：`DialogHost` 由 M9-07 的 main 注入
 *   `dialog.showOpenDialog`，测试注入替身；
 * - 错误形状与 `aether-app` 结构化错误同字段（`code`/`message`/`field`），
 *   便于 typed client 统一处理。
 */
import { realpathSync } from "node:fs";
import path from "node:path";

/** `ref_pick` 方法名（命令面 63 条之一，路由 = shell）。 */
export const REF_PICK_METHOD = "ref_pick";
/** `startup_pick_target` 方法名（命令面 63 条之一，路由 = shell）。 */
export const STARTUP_PICK_TARGET_METHOD = "startup_pick_target";
/** 壳本地命令集合（与 `aether-app::SHELL_COMMANDS` 同名）。 */
export const SHELL_LOCAL_METHODS = [REF_PICK_METHOD, STARTUP_PICK_TARGET_METHOD] as const;
/** 路径字符串上限（与 `aether-app` 的 `MAX_PATH_CHARS` 同口径）。 */
export const MAX_PATH_CHARS = 4_096;

export type ShellLocalMethod = (typeof SHELL_LOCAL_METHODS)[number];
export type PickKind = "file" | "directory";

/** `dialog.showOpenDialog` 的最小选项面（ADR-029 §4.6）。 */
export interface OpenDialogOptions {
  properties: Array<"openFile" | "openDirectory">;
  title?: string;
}

/** `dialog.showOpenDialog` 的最小结果面。 */
export interface OpenDialogResult {
  canceled: boolean;
  filePaths: readonly string[];
}

/**
 * 原生对话框宿主（生产 = Electron `dialog`；测试 = 替身）。
 *
 * 只依赖本接口可保证 `electron` 尚未加入依赖前（M9-06）可单测；
 * M9-07 在 main 中注入真实实现。
 */
export interface DialogHost {
  showOpenDialog(options: OpenDialogOptions): Promise<OpenDialogResult>;
}

/** 结构化错误（与 `aether-app::IpcError` 的 `code`/`message`/`field` 同形）。 */
export interface ShellError {
  code: string;
  message: string;
  field?: string;
}

/** 壳本地命令结果（main 转成 renderer 的 `{ result }` / `{ error }`）。 */
export type ShellCommandOutcome =
  | { ok: true; result: { path: string | null } | { target_dir: string | null } }
  | { ok: false; error: ShellError };

function fail(code: string, message: string, field?: string): { ok: false; error: ShellError } {
  return { ok: false, error: field === undefined ? { code, message } : { code, message, field } };
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasControlChars(value: string): boolean {
  // 与 Rust `char::is_control` 同口径（C0/C1 + DEL）。
  return /[\u0000-\u001f\u007f-\u009f]/.test(value);
}

/**
 * 把选择器结果规范化为 canonical 绝对路径。
 *
 * - 取消（`canceled` 或空列表）→ `{ value: null }`；
 * - 相对路径 / 不存在 / canonicalize 失败 → `path_rejected`（不返回不可信路径）；
 * - 成功 → `fs.realpathSync.native`（解析软链接/Junction；Windows 长路径形态）。
 */
function canonicalizePicked(
  picked: string,
  field: "path" | "target_dir",
): { ok: true; value: string } | { ok: false; error: ShellError } {
  if (!path.isAbsolute(picked)) {
    return fail("path_rejected", "选择器必须返回绝对路径（相对路径不受信任）", field);
  }
  if (picked.length > MAX_PATH_CHARS) {
    return fail("too_large", `路径字符数 ${picked.length} 超过上限 ${MAX_PATH_CHARS}`, field);
  }
  if (hasControlChars(picked)) {
    return fail("invalid_format", "路径包含不允许的控制字符", field);
  }
  let canonical: string;
  try {
    canonical = realpathSync.native(picked);
  } catch {
    return fail("path_rejected", "路径不可用（canonicalize 失败或不存在）", field);
  }
  if (!path.isAbsolute(canonical)) {
    return fail("path_rejected", "canonicalize 结果不是绝对路径", field);
  }
  return { ok: true, value: canonical };
}

async function openPicker(
  dialog: DialogHost,
  kind: PickKind,
  field: "path" | "target_dir",
): Promise<{ ok: true; value: string | null } | { ok: false; error: ShellError }> {
  let result: OpenDialogResult;
  try {
    result = await dialog.showOpenDialog({
      properties: [kind === "file" ? "openFile" : "openDirectory"],
    });
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    return fail("internal", `系统选择器错误：${detail}`);
  }
  if (result.canceled) {
    return { ok: true, value: null };
  }
  const first = result.filePaths[0];
  if (first === undefined) {
    return { ok: true, value: null };
  }
  if (typeof first !== "string" || first.length === 0) {
    return fail("invalid_value", "选择器返回空路径", field);
  }
  const canonical = canonicalizePicked(first, field);
  if (!canonical.ok) {
    return canonical;
  }
  return { ok: true, value: canonical.value };
}

function parseRefPickParams(
  params: unknown,
): { ok: true; kind: PickKind } | { ok: false; error: ShellError } {
  if (!isPlainObject(params)) {
    return fail("invalid_json", "命令参数必须是 JSON 对象（严格模式拒绝 null/数组/标量）");
  }
  const unknown = Object.keys(params).find((key) => key !== "kind");
  if (unknown !== undefined) {
    return fail("unknown_field", `未知字段 ${unknown}`, unknown);
  }
  if (!("kind" in params)) {
    return fail("missing_field", "缺少必填字段 kind", "kind");
  }
  const kind = params.kind;
  if (typeof kind !== "string") {
    return fail("invalid_type", "kind 必须是字符串（file/directory）", "kind");
  }
  if (kind !== "file" && kind !== "directory") {
    return fail("invalid_enum", `kind 只允许 file/directory（收到 ${JSON.stringify(kind)}）`, "kind");
  }
  return { ok: true, kind };
}

/** `startup_pick_target`：无参数（`null`/缺省/`{}` 等价；任何成员拒绝）。 */
function parseNoParams(params: unknown): { ok: true } | { ok: false; error: ShellError } {
  if (params === null || params === undefined) {
    return { ok: true };
  }
  if (!isPlainObject(params)) {
    return fail("invalid_json", "命令参数必须是 JSON 对象（严格模式拒绝数组/标量）");
  }
  const unknown = Object.keys(params)[0];
  if (unknown !== undefined) {
    return fail("unknown_field", `未知字段 ${unknown}`, unknown);
  }
  return { ok: true };
}

/**
 * 壳本地命令入口（main 的路由表命中 4 条 shell-local 时调用）。
 *
 * 未知方法返回 `not_implemented`（与 `aether-app::dispatch` 同码）。
 */
export async function handleShellLocalCommand(
  method: string,
  params: unknown,
  dialog: DialogHost,
): Promise<ShellCommandOutcome> {
  if (method === REF_PICK_METHOD) {
    const parsed = parseRefPickParams(params);
    if (!parsed.ok) {
      return parsed;
    }
    const picked = await openPicker(dialog, parsed.kind, "path");
    if (!picked.ok) {
      return picked;
    }
    return { ok: true, result: { path: picked.value } };
  }
  if (method === STARTUP_PICK_TARGET_METHOD) {
    const parsed = parseNoParams(params);
    if (!parsed.ok) {
      return parsed;
    }
    const picked = await openPicker(dialog, "directory", "target_dir");
    if (!picked.ok) {
      return picked;
    }
    return { ok: true, result: { target_dir: picked.value } };
  }
  return fail("not_implemented", `未知壳本地命令 ${method}`);
}
