/**
 * Agent 预设 IPC 契约（M7-09；ADR-026 §2.2；设计 D7/D14）。
 *
 * 与 Rust 侧 `preset_control.rs` 的 JSON 形状一致（snake_case）：
 * - `presets_list` → 预设清单（内置在前、创建时间升序；无参数，`payload` 缺省 / `null`）；
 * - `preset_add` / `preset_update` → 新增 / 更新（名称 ≤64 字符、提示词 ≤32KiB）；
 * - `preset_delete` → 删除（**内置不可删**：`builtin_preset_undeletable`；内置可编辑）。
 *
 * 预设提示词与工作区记忆注入的合成由核心完成（预设块置前、记忆块后置，禁止静默覆盖）；
 * 本文件是预设管理页与新建会话入口的类型化封装（可注入替身；生产 = Tauri invoke）。
 */
import { invoke } from "@tauri-apps/api/core";

/** `presets_list` 元素（`presets` 表投影；字段与表列一致）。 */
export interface PresetEntry {
  id: string;
  name: string;
  system_prompt: string;
  /** 内置标记（`is_builtin=1`；内置可编辑、**不可删除**）。 */
  is_builtin: boolean;
  created_at: number;
  updated_at: number;
}

/** `preset_add` 参数（ADR-026 §2.2-2）。 */
export interface PresetAddInput {
  name: string;
  system_prompt: string;
}

/** `preset_update` 参数（至少 1 个变更字段；内置可编辑）。 */
export interface PresetUpdateInput {
  preset_id: string;
  name?: string;
  system_prompt?: string;
}

/** 预设命令面（可注入替身；生产 = Tauri 生产实现）。 */
export interface PresetsIpc {
  /** 预设清单（无参数；`payload` 缺省 / `null` 合法）。 */
  listPresets(): Promise<PresetEntry[]>;
  addPreset(input: PresetAddInput): Promise<PresetEntry>;
  updatePreset(input: PresetUpdateInput): Promise<PresetEntry>;
  deletePreset(presetId: string): Promise<{ preset_id: string; deleted: boolean }>;
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const presetsIpc: PresetsIpc = {
  async listPresets() {
    const result = await invoke<{ presets: PresetEntry[] }>("presets_list", { payload: null });
    return result.presets;
  },
  async addPreset(input) {
    return invoke<PresetEntry>("preset_add", { payload: input });
  },
  async updatePreset(input) {
    return invoke<PresetEntry>("preset_update", { payload: input });
  },
  async deletePreset(presetId) {
    return invoke<{ preset_id: string; deleted: boolean }>("preset_delete", {
      payload: { preset_id: presetId },
    });
  },
};

/** 预设名称上限（ADR-026 §2.2-2：≤64 字符；与核心常量同值）。 */
export const PRESET_NAME_MAX_CHARS = 64;
/** 预设系统提示词上限（ADR-026 §2.2-2：≤32KiB；与核心常量同值）。 */
export const PRESET_SYSTEM_PROMPT_MAX_BYTES = 32 * 1024;

/** 预设不存在错误码（ADR-026 附录 A-2；UI-UX §5 文案「预设不存在，已刷新列表」）。 */
export const PRESET_NOT_FOUND_CODE = "preset_not_found";
/** 内置预设不可删除错误码（ADR-026 附录 A-2；UI-UX §5 文案「内置预设不可删除」）。 */
export const BUILTIN_PRESET_UNDELETABLE_CODE = "builtin_preset_undeletable";

/** `preset_not_found` 展示文案（UI-UX §5 文案表登记值）。 */
export const PRESET_NOT_FOUND_MESSAGE = "预设不存在，已刷新列表";
/** `builtin_preset_undeletable` 展示文案（UI-UX §5 文案表登记值）。 */
export const BUILTIN_PRESET_UNDELETABLE_MESSAGE = "内置预设不可删除";

/**
 * 预设提示词与记忆注入的合成分隔标记前缀（ADR-026 附录 A-4）。
 *
 * 与核心 `PRESET_PROMPT_MARKER_PREFIX` 同值：`<!-- aether:preset:<preset_id> -->`。
 */
export const PRESET_PROMPT_MARKER_PREFIX = "<!-- aether:preset:";

/** 预设提示词块标记（合成结果回显/断言用；`session.system_prompt` 内定位）。 */
export function presetPromptMarker(presetId: string): string {
  return `${PRESET_PROMPT_MARKER_PREFIX}${presetId} -->`;
}

/** 名称长度校验（Unicode 标量值计数；与核心 `ensure_max_chars` 同口径）。 */
export function presetNameTooLong(name: string): boolean {
  return [...name].length > PRESET_NAME_MAX_CHARS;
}

/** 提示词长度校验（UTF-8 字节数；与核心 `ensure_max_bytes` 同口径）。 */
export function presetPromptTooLarge(systemPrompt: string): boolean {
  return new TextEncoder().encode(systemPrompt).length > PRESET_SYSTEM_PROMPT_MAX_BYTES;
}
