/**
 * 容量状态辅助（M3-05；设计 D13：2GB 警告 / 5GB 强烈提示）。
 *
 * 阈值与等级由核心投影（`backup_list.capacity`；参数化注入，前端只做文案）。
 */
import { formatBytes, type BackupCapacity } from "./backup";
import { t } from "./i18n";

/** 容量等级中文文案（横幅标题）。 */
export function capacityLevelLabel(level: BackupCapacity["level"]): string {
  switch (level) {
    case "critical":
      return t("capacity.capacity_critical");
    case "warn":
      return t("capacity.capacity_warning");
    default:
      return t("capacity.capacity_normal");
  }
}

/** 容量摘要文案（含阈值提示）。 */
export function capacitySummary(capacity: BackupCapacity | null): string {
  if (capacity === null) {
    return t("app.common.checking_capacity");
  }
  return (
    t("backup.database", { p0: formatBytes(capacity.total_bytes) }) +
    t("backup.db_wal", { p0: formatBytes(capacity.db_bytes), p1: formatBytes(capacity.wal_bytes) }) +
    t("backup.warning_threshold_critical", { p0: formatBytes(capacity.warn_bytes), p1: formatBytes(capacity.critical_bytes) })
  );
}
