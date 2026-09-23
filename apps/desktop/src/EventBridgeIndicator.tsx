/**
 * 事件通道状态指示（M3-01 / D7）：`aether://event` 监听注册状态。
 *
 * `listening` = 已注册监听（可接收核心广播）；`unavailable` = 非 Tauri 环境或
 * 注册失败（仅本地开发/E2E 快速失败，不影响其余 UI）。
 */
import { appEventStore } from "./aetherStore";
import { useAetherEventBridge } from "./eventBridge";
import type { EventStore } from "./eventStore";

const STATUS_TEXT: Record<string, string> = {
  connecting: "连接中…",
  listening: "已连接（aether://event）",
  unavailable: "不可用",
};

export function EventBridgeIndicator({
  store = appEventStore,
}: {
  store?: EventStore;
}) {
  const status = useAetherEventBridge(store);
  return (
    <p className="event-bridge" data-testid="event-bridge-status">
      事件通道：{STATUS_TEXT[status] ?? status}
    </p>
  );
}
