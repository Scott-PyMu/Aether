#!/usr/bin/env node
/**
 * Windows Electron 45 ACL 修复（仅用于本机 GUI 截图/E2E）。
 *
 * 背景：Windows 11 build 26200 上，若 Electron 安装目录 DACL 缺少
 * `S-1-15-2-2`（ALL RESTRICTED APPLICATION PACKAGES）或含异常 SID，
 * GPU 进程以 `-2147483645` 退出，主进程随后 `0xc0000005`。
 * 参考 electron/electron#51761 / #52255。
 *
 * 做法：复制 Electron 45 官方 dist 到 `%TEMP%/aether-electron45-fixed`，
 * 仅对该临时副本执行 `icacls /reset` + 授予受限应用包读取/执行权限；
 * 不修改仓库、node_modules 或用户安装目录。
 */
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

export const FIXED_ELECTRON_DIR = path.join(os.tmpdir(), "aether-electron45-fixed");

export function resolveFixedElectron(repoRoot) {
  const source = path.join(
    repoRoot,
    "node_modules",
    ".pnpm",
    "electron@45.0.0",
    "node_modules",
    "electron",
    "dist",
  );
  const exe = path.join(FIXED_ELECTRON_DIR, process.platform === "win32" ? "electron.exe" : "electron");
  if (process.platform !== "win32") {
    return exe;
  }
  if (!existsSync(exe)) {
    mkdirSync(FIXED_ELECTRON_DIR, { recursive: true });
    cpSync(source, FIXED_ELECTRON_DIR, { recursive: true });
  }
  try {
    execFileSync("icacls", [FIXED_ELECTRON_DIR, "/reset", "/t", "/c"], {
      stdio: "ignore",
      windowsHide: true,
    });
    execFileSync("icacls", [FIXED_ELECTRON_DIR, "/grant", "*S-1-15-2-2:(OI)(CI)(RX)"], {
      stdio: "ignore",
      windowsHide: true,
    });
  } catch {
    // 非管理员/非 Windows ACL 工具不可用时保留副本；失败会在启动阶段暴露。
  }
  return exe;
}
