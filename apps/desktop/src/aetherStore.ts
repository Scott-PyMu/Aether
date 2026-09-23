/**
 * 应用级 EventStore 单例（M3-01）。
 *
 * 生产补读源（`messages_page` IPC）随 M3-02 会话命令装配接线；当前不注入
 * 数据源（缺口保留 `gap` 状态、>10k 触发「历史消息过多」提示），缺口登记于
 * `docs/M3-01-证据.md`。
 */
import { EventStore } from "./eventStore";

/** 全应用共享的事件流存储（单例；测试请自行构造 EventStore 注入替身）。 */
export const appEventStore = new EventStore();
