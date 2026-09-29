/**
 * 容量状态辅助（M3-05；设计 D13：2GB 警告 / 5GB 强烈提示）。
 *
 * 阈值与等级由核心投影（`backup_list.capacity`；参数化注入，前端只做文案）。
 */
import { formatBytes, type BackupCapacity } from "./backup";

/** 容量等级中文文案（横幅标题）。 */
export function capacityLevelLabel(level: BackupCapacity["level"]): string {
  switch (level) {
    case "critical":
      return "容量紧张";
    case "warn":
      return "容量警告";
    default:
      return "容量正常";
  }
}

/** 容量摘要文案（含阈值提示）。 */
export function capacitySummary(capacity: BackupCapacity | null): string {
  if (capacity === null) {
    return "容量查询中…";
  }
  return (
    `数据库 ${formatBytes(capacity.total_bytes)}` +
    `（db ${formatBytes(capacity.db_bytes)} + wal ${formatBytes(capacity.wal_bytes)}）` +
    `；警告阈值 ${formatBytes(capacity.warn_bytes)} / 强提示 ${formatBytes(capacity.critical_bytes)}`
  );
}
