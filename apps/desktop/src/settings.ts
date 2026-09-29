/**
 * 设置项 IPC 契约与语义（M3-05；设计 D13、UI-UX S-05/Q9/Q10）。
 *
 * 当前登记键（核心白名单 `SETTINGS_KEY_ALLOWLIST`）：
 * - `backup.reminder`：7 天未备份提醒开关（布尔；缺省 `true`，全局粒度）。
 *
 * 未登记键由核心返回 `invalid_enum`（默认拒绝）。工作区绑定键归 M3-08。
 */
import { invoke } from "@tauri-apps/api/core";

/** 备份提醒开关设置键（M3-05 登记；稳定契约）。 */
export const BACKUP_REMINDER_KEY = "backup.reminder";

/** 未备份提醒状态（`backup_list.reminder` 投影；核心时钟计算，可注入）。 */
export interface BackupReminder {
  enabled: boolean;
  due: boolean;
  last_backup_at: number | null;
  since_ms: number | null;
  threshold_ms: number;
  /** `never`（从未备份）/ `stale`（超阈值）/ null（未到期或已关闭）。 */
  reason: "never" | "stale" | null;
}

export interface SettingsGetResult {
  key: string;
  value: unknown;
}

export interface SettingsIpc {
  get(key: string): Promise<SettingsGetResult>;
  set(key: string, value: unknown): Promise<SettingsGetResult>;
}

/** 生产实现（Tauri IPC）。 */
export const settingsIpc: SettingsIpc = {
  async get(key) {
    return invoke<SettingsGetResult>("settings_get", { payload: { key } });
  },
  async set(key, value) {
    return invoke<SettingsGetResult>("settings_set", {
      payload: { key, value },
    });
  },
};
