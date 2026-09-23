/**
 * zustand 订阅钩子（M3-01）：把 {@link EventStore} 的每会话 vanilla store
 * 暴露为 React hook（选择器可选）。
 */
import { useStore } from "zustand";

import type { EventStore, SessionEventsState } from "./eventStore";

/** 订阅指定会话的事件视图状态。 */
export function useSessionEvents(
  store: EventStore,
  sessionId: string,
): SessionEventsState {
  return useStore(store.session(sessionId));
}
