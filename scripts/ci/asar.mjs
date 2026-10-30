/**
 * ASAR 头部读取（M9-11；仅 Node 标准库）。
 *
 * 用于打包断言：`aetherd`/适配器**不得**在 asar 内（必须走 `extraResources`）；
 * renderer/main 入口必须在 asar 内。ASAR 头是 Chromium Pickle：
 *   0..4   = uint32LE 4（pickle 载荷长度）
 *   4..8   = uint32LE headerSize
 *   8..12  = uint32LE 4
 *   12..16 = uint32LE headerSize
 *   16..   = header JSON（UTF-8）
 * 为兼容不同写入器，读取时同时校验两种布局。
 */
import { openSync, readSync, closeSync, fstatSync } from "node:fs";

/** 读取 asar 头 JSON；失败抛错（打包产物必须可读）。 */
export function readAsarHeader(file) {
  const fd = openSync(file, "r");
  try {
    const size = fstatSync(fd).size;
    const probe = Buffer.alloc(16);
    if (size < 16) throw new Error(`asar 文件过短：${file}`);
    readSync(fd, probe, 0, 16, 0);
    const first = probe.readUInt32LE(0);
    const declared = probe.readUInt32LE(4);
    // 实测布局（electron-builder 26 / Electron 45）：0..4=4，4..8=pickle 载荷长度，
    // 8..12=JSON 长度+6，12..16=JSON 长度，16..=JSON。
    let headerSize = probe.readUInt32LE(12);
    let start = 16;
    if (!(first === 4 && headerSize > 0 && start + headerSize <= size)) {
      headerSize = declared;
      start = 8;
    }
    if (headerSize <= 0 || start + headerSize > size) {
      throw new Error(`asar 头长度非法（${headerSize}）：${file}`);
    }
    const header = Buffer.alloc(headerSize);
    readSync(fd, header, 0, headerSize, start);
    const text = header.toString("utf8");
    try {
      return JSON.parse(text);
    } catch (error) {
      // 兜底：兼容 8 字节起始的旧写入器。
      if (start === 16) {
        const altSize = probe.readUInt32LE(4);
        if (altSize > 0 && 8 + altSize <= size) {
          const alt = Buffer.alloc(altSize);
          readSync(fd, alt, 0, altSize, 8);
          return JSON.parse(alt.toString("utf8"));
        }
      }
      throw new Error(`asar 头 JSON 解析失败：${file}（${error.message}）`);
    }
  } finally {
    closeSync(fd);
  }
}

/** 递归列出 asar 内文件路径（`/` 分隔；不含目录）。 */
export function listAsarFiles(file) {
  const header = readAsarHeader(file);
  const out = [];
  const walk = (node, prefix) => {
    for (const [name, value] of Object.entries(node?.files ?? {})) {
      const full = `${prefix}/${name}`;
      if (value && typeof value === "object" && value.files) {
        walk(value, full);
      } else if (value && typeof value === "object" && typeof value.size === "number") {
        out.push(full);
      }
    }
  };
  walk(header, "");
  return out.sort();
}

/** asar 内是否包含指定 basename（大小写不敏感；用于 sidecar 泄漏断言）。 */
export function asarContainsBasename(files, basename) {
  const lower = basename.toLowerCase();
  return files.some((entry) => entry.toLowerCase().split("/").pop() === lower);
}
