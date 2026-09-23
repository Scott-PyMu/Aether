/**
 * `aether://event` 监听接线（M3-01 / D7 单通道）。
 *
 * 核心侧事件桥把落盘后的事件广播到单通道 `aether://event`（信封含 `session_id`，
 * UI 侧过滤）；本模块把生成绑定的类型化监听器接到 {@link EventStore}。
 *
 * 生产监听经 tauri-specta 生成物（`packages/protocol/src/bindings.ts` 的
 * `events.aetherEvent.listen`，禁止手改，AGENTS §2.8）；测试注入替身监听器。
 */
import type { AetherEvent } from "@aether/protocol";
import { useEffect, useState } from "react";

import type { EventStore } from "./eventStore";

/** 单条事件回调。 */
export type AetherEventHandler = (event: AetherEvent) => void;

/** 监听注册函数（返回解除函数）。 */
export type AetherEventListener = (
  handler: AetherEventHandler,
) => Promise<() => void>;

/** 生产监听器：经生成绑定订阅单通道。 */
export async function defaultAetherEventListener(
  handler: AetherEventHandler,
): Promise<() => void> {
  const { events } = await import("@aether/protocol");
  const unlisten = await events.aetherEvent.listen((event) => {
    handler(event.payload);
  });
  return unlisten;
}

/** 注册到 EventStore（返回解除函数）。 */
export async function registerAetherEventListener(
  store: EventStore,
  listen: AetherEventListener = defaultAetherEventListener,
): Promise<() => void> {
  return listen((event) => {
    store.ingest(event);
  });
}

export type EventBridgeStatus = "connecting" | "listening" | "unavailable";

/** React 钩子：挂载时注册监听，卸载时解除。 */
export function useAetherEventBridge(
  store: EventStore,
  listen: AetherEventListener = defaultAetherEventListener,
): EventBridgeStatus {
  const [status, setStatus] = useState<EventBridgeStatus>("connecting");

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | null = null;
    registerAetherEventListener(store, listen)
      .then((dispose) => {
        if (active) {
          unlisten = dispose;
          setStatus("listening");
        } else {
          dispose();
        }
      })
      .catch(() => {
        if (active) {
          setStatus("unavailable");
        }
      });
    return () => {
      active = false;
      if (unlisten) {
        unlisten();
      }
    };
  }, [store, listen]);

  return status;
}
