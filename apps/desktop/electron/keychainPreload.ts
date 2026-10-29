/**
 * S-13 Keychain 重授权页专用 preload（UI-UX v0.17；ADR-029 §5.4）。
 *
 * 与主 UI `window.aether`、S-12 `window.aetherFatal` 分离；只暴露本页所需动作。
 * 触发与真实 Keychain preflight 由 M9-09 接线（本任务只交付页面/锚点/回执形状）。
 */
import { contextBridge, ipcRenderer } from "electron";

import {
  KEYCHAIN_PASSPHRASE_CHANNEL,
  KEYCHAIN_RETRY_CHANNEL,
  KEYCHAIN_SKIP_CHANNEL,
  KEYCHAIN_STATE_CHANNEL,
} from "./shellPageChannels";

export interface KeychainViewState {
  scope: "provider" | "backup";
  mode: "reauth" | "passphrase" | "degraded";
  reason: string;
  notice?: string;
}

export interface KeychainActionResult {
  ok: boolean;
  error?: string;
}

const api = {
  getState(): Promise<KeychainViewState> {
    return ipcRenderer.invoke(KEYCHAIN_STATE_CHANNEL) as Promise<KeychainViewState>;
  },
  retry(): Promise<KeychainActionResult> {
    return ipcRenderer.invoke(KEYCHAIN_RETRY_CHANNEL) as Promise<KeychainActionResult>;
  },
  submitPassphrase(passphrase: string): Promise<KeychainActionResult> {
    return ipcRenderer.invoke(KEYCHAIN_PASSPHRASE_CHANNEL, passphrase) as Promise<KeychainActionResult>;
  },
  skip(): Promise<KeychainActionResult> {
    return ipcRenderer.invoke(KEYCHAIN_SKIP_CHANNEL) as Promise<KeychainActionResult>;
  },
};

contextBridge.exposeInMainWorld("aetherKeychain", Object.freeze(api));
