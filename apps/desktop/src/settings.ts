/**
 * 设置项 IPC 契约与语义（M3-05；设计 D13、UI-UX S-05/Q9/Q10；M5-02 自动备份键）。
 *
 * 当前登记键（核心白名单 `SETTINGS_KEY_ALLOWLIST`）：
 * - `backup.reminder`：7 天未备份提醒开关（布尔；缺省 `true`，全局粒度）；
 * - `backup.auto.enabled` / `backup.auto.frequency` / `backup.auto.retention`：
 *   自动备份总开关 / 频率（daily|weekly）/ 自动产物保留份数（1–100，缺省 10；
 *   手动产物不自动删除）——M5-02/ADR-020。
 *
 * 未登记键由核心返回 `invalid_enum`（默认拒绝）。工作区绑定键归 M3-08。
 */
import { invoke } from "@tauri-apps/api/core";

/** 备份提醒开关设置键（M3-05 登记；稳定契约）。 */
export const BACKUP_REMINDER_KEY = "backup.reminder";
/** 自动备份总开关设置键（M5-02/ADR-020；缺省开启）。 */
export const BACKUP_AUTO_ENABLED_KEY = "backup.auto.enabled";
/** 自动备份频率设置键（M5-02/ADR-020；`daily` 缺省 / `weekly`）。 */
export const BACKUP_AUTO_FREQUENCY_KEY = "backup.auto.frequency";
/** 自动备份保留份数设置键（M5-02/ADR-020；1–100，缺省 10）。 */
export const BACKUP_AUTO_RETENTION_KEY = "backup.auto.retention";

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
