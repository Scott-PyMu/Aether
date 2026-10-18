/**
 * 设置项 IPC 契约与语义（M3-05；设计 D13、UI-UX S-05/Q9/Q10；M5-02 自动备份键；
 * M5-03 加密备份键；M5-04 事件保留键）。
 *
 * 当前登记键（核心白名单 `SETTINGS_KEY_ALLOWLIST`）：
 * - `backup.reminder`：7 天未备份提醒开关（布尔；缺省 `true`，全局粒度）；
 * - `backup.auto.enabled` / `backup.auto.frequency` / `backup.auto.retention`：
 *   自动备份总开关 / 频率（daily|weekly）/ 自动产物保留份数（1–100，缺省 10；
 *   手动产物不自动删除）——M5-02/ADR-020；
 * - `backup.encryption.enabled`：加密备份开关（布尔；缺省 `true`；`true` 且主密钥
 *   不可用 → 备份暂停并告警，禁止静默降级为明文）——M5-03/ADR-020；
 * - `events.retention.enabled` / `events.retention.days`：事件保留策略开关（缺省
 *   `false`；2GB 提示、增长 >500MB/月或 ≥5GB 自动置 `true`）/ 保留窗口（7–365 天，
 *   缺省 30）——M5-04/ADR-021；
 * - `ui.layout`：工作台多视图布局（JSON 对象字符串 ≤64KiB，缺省 `"{}"`；
 *   标签页/分屏/聚焦窗格；损坏回退默认布局）——M7-04/ADR-018 附录 A-6。
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
/** 加密备份开关设置键（M5-03/ADR-020；缺省 `true`）。 */
export const BACKUP_ENCRYPTION_ENABLED_KEY = "backup.encryption.enabled";
/** 事件保留策略开关设置键（M5-04/ADR-021；缺省关闭；阈值自动置 true）。 */
export const EVENTS_RETENTION_ENABLED_KEY = "events.retention.enabled";
/** 事件保留窗口设置键（M5-04/ADR-021；7–365 天，缺省 30）。 */
export const EVENTS_RETENTION_DAYS_KEY = "events.retention.days";
/** 工作台布局持久化设置键（M7-04/ADR-018 附录 A-6；JSON 对象字符串，缺省 `{}`）。 */
export const UI_LAYOUT_KEY = "ui.layout";

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
