# ADR-040：启动检查按脏分片选择性执行

- 状态：**Proposed（2026-11-16，待四角色签署；未实施）**
- 关联：ADR-039 v1.0 D-039-2/D-039-3；M10-10
- 决策类型：实现优化（不改变事件/分片/备份契约）
- **M10-10 实施口径（2026-11-16）**：本 ADR 未签署，产品仍按 ADR-039 v1.0
  「任一指纹不匹配 → 主库 + 全部分片」执行；本文件仅保留提案，不构成实现依据。

## 1. 背景

ADR-039 D-039-2 规定：严格 sidecar 全匹配时跳过启动 `quick_check`；否则检查主库 + 全部分片。
5.22 GiB / 8 分片在 SATA SSD 上的实测显示：

- 顺序单片 `quick_check` 约 5–13s；
- 8 并发墙钟约 31.69s（未 mmap）/10.64s（mmap 512MiB）；
- 冷缓存、系统索引/UI I/O 叠加时曾出现 34.6s，超过 30s 硬上限。

多数异常退出只影响最近写入的 1–2 个分片，全部 8 片重扫没有必要。

## 2. 决策

在 sidecar 存在、generation/app_version/关闭阶段完整时，按数据库文件指纹选择性检查：

1. 主库与每个分片记录：`schema_version`、`header_change_counter`、`file_size`、`wal_size`、`mtime`、`last_seq`；
2. 启动时仅对**指纹变化**的分片执行 `quick_check`；出现任一脏分片时附带检查主库（主库很小）；
3. 所有指纹匹配 → 跳过启动 `quick_check`（原严格 sidecar 行为）；
4. sidecar 缺失、版本变化、关闭阶段不完整、分片缺少指纹 → 退化为主库 + 全部分片；
5. 周检/每 20 次启动执行完整 `integrity_check`（主库 + 全部分片，后台，不关闭写门）；失败才 `persist_degraded`；
6. 检查期间 `health.storage_check` 保持 `checking`，写门规则不变；未检查分片由周期完整检查兜底。

## 3. 安全论证

- 指纹包含 SQLite header change counter、文件大小、WAL 大小和 mtime；正常写入至少改变其中之一；
- 位翻转等不改变上述字段的磁盘损坏不会被选择性检查发现，由周检/20 次完整 `integrity_check` 兜底；
- sidecar 缺失或任何字段不一致时绝不跳过全量检查；
- 不改变事件表、分片路由、`CURRENT`、备份恢复协议。

## 4. 验证（签署后实施时）

- `plan_startup_checks` 单测：干净 sidecar 严格跳过；修改单一分片后仅该分片进入计划；
- `m10_10_startup`：5.22 GiB generation 启动检查 15.78/15.93s（mmap 512MiB）；
- `m10_10_periodic`：20 次启动触发后台 `integrity_check`，成功后 `last_full_check_at` 更新、`clean_start_count=0`；
- `pnpm verify:m10-10`。

## 5. 后续

四角色签署后更新设计文档版本/ADR 清单。**未签署前按 ADR-039 v1.0 全量检查执行**，
G13 以 ADR-039 字面与 M10-10 证据为准；本 ADR 不进入实现/G13 判定。
