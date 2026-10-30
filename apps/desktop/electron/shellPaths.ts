/**
 * 壳级页面（S-00/S-12/S-13）路径解析（M9-11 打包布局）。
 *
 * 背景：打包后 `main/preload` 从 `app.asar` 加载（Node `fs` 原生支持 asar），但
 * `BrowserWindow.loadFile()` 经 Chromium `file://` 协议读取 **asar 内文件**在
 * Electron 45 打包产物上返回 `ERR_FILE_NOT_FOUND`。因此壳级页面通过
 * `asarUnpack` 落到 `resources/app.asar.unpacked/...`，并优先从该目录加载。
 *
 * 纯函数（不依赖 Electron API），便于单测锁定打包/开发两种布局。
 */
import path from "node:path";

export interface ShellAssetInput {
  page: string;
  file: string;
  /** `app.isPackaged`。 */
  isPackaged: boolean;
  /** `process.resourcesPath`（打包后）。 */
  resourcesPath?: string;
  /** `__dirname`（main.cjs 所在目录）。 */
  dirname: string;
}

/** 返回按优先级排列的候选路径（第一个存在的由调用方选用）。 */
export function shellAssetCandidates(input: ShellAssetInput): string[] {
  const candidates: string[] = [];
  if (input.isPackaged && input.resourcesPath) {
    candidates.push(
      path.join(
        input.resourcesPath,
        "app.asar.unpacked",
        "apps",
        "desktop",
        "electron",
        "dist",
        "shell",
        input.page,
        input.file,
      ),
    );
  }
  candidates.push(path.join(input.dirname, "shell", input.page, input.file));
  candidates.push(path.join(input.dirname, "..", input.page, input.file));
  return candidates;
}
