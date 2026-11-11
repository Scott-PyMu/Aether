/**
 * M10-05/ADR-035 缩略图解码 UtilityProcess（受限进程，非沙箱）。
 *
 * 只做代码约束：无网络/无 shell；解码 PNG/JPEG/WebP/GIF 首帧 → 512px WebP q80，
 * EXIF rotate、剥离元数据。像素/边长上限在构造解码器前检查（ADR D-035-1）。
 *
 * 该文件**不参与 Vite 打包**（必须保持外部 require，native `.node` 经 asarUnpack）。
 * 主进程经 `utilityProcess.fork` 启动，消息协议：
 *   in : { id, sourcePath, thumbPath }
 *   out: { id, ok, error? }
 */
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const MAX_PIXELS = 25_000_000;
const MAX_EDGE = 8192;
const THUMB_EDGE = 512;

function requireFromRoots(name) {
  const roots = [
    path.join(__dirname, "..", "..", "native-runtime", "node_modules"),
    path.join(__dirname, "..", "native-runtime", "node_modules"),
    path.join(__dirname, "..", "..", "node_modules"),
    path.join(__dirname, "..", "node_modules"),
  ];
  let lastError = null;
  for (const root of roots) {
    try {
      return require(path.join(root, name));
    } catch (error) {
      lastError = error;
    }
  }
  throw lastError ?? new Error(`cannot resolve ${name}`);
}

const { Transformer } = requireFromRoots("@napi-rs/image");
const { GifReader } = requireFromRoots("omggif");

function sniff(bytes) {
  if (
    bytes.length >= 24 &&
    bytes[0] === 0x89 &&
    bytes.toString("ascii", 1, 4) === "PNG" &&
    bytes.toString("ascii", 12, 16) === "IHDR"
  ) {
    return { format: "png", width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
  }
  if (bytes.length >= 10) {
    const magic = bytes.toString("ascii", 0, 6);
    if (magic === "GIF87a" || magic === "GIF89a") {
      return { format: "gif", width: bytes.readUInt16LE(6), height: bytes.readUInt16LE(8) };
    }
  }
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) {
    let offset = 2;
    while (offset + 4 <= bytes.length) {
      if (bytes[offset] !== 0xff) {
        offset += 1;
        continue;
      }
      while (offset < bytes.length && bytes[offset] === 0xff) offset += 1;
      const marker = bytes[offset];
      offset += 1;
      if ((marker >= 0xd0 && marker <= 0xd9) || marker === 0x01) continue;
      if (offset + 2 > bytes.length) break;
      const length = bytes.readUInt16BE(offset);
      if (length < 2 || offset + length > bytes.length) break;
      const isSof = [0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker);
      if (isSof && length >= 7) {
        return { format: "jpeg", height: bytes.readUInt16BE(offset + 3), width: bytes.readUInt16BE(offset + 5) };
      }
      offset += length;
    }
    return null;
  }
  if (
    bytes.length >= 21 &&
    bytes.toString("ascii", 0, 4) === "RIFF" &&
    bytes.toString("ascii", 8, 12) === "WEBP"
  ) {
    const chunk = bytes.toString("ascii", 12, 16);
    if (chunk === "VP8 " && bytes[23] === 0x9d && bytes[24] === 0x01 && bytes[25] === 0x2a) {
      return { format: "webp", width: bytes.readUInt16LE(26) & 0x3fff, height: bytes.readUInt16LE(28) & 0x3fff };
    }
    if (chunk === "VP8L" && bytes[20] === 0x2f) {
      const bits = bytes.readUInt32LE(21);
      return { format: "webp", width: (bits & 0x3fff) + 1, height: ((bits >>> 14) & 0x3fff) + 1 };
    }
    if (chunk === "VP8X") {
      const width = bytes.readUIntLE(24, 3) + 1;
      const height = bytes.readUIntLE(27, 3) + 1;
      return { format: "webp", width, height };
    }
  }
  return null;
}

async function thumbnail(bytes) {
  const header = sniff(bytes);
  if (!header || !header.width || !header.height) {
    throw new Error("attachment_type_unsupported");
  }
  if (header.width * header.height > MAX_PIXELS || Math.max(header.width, header.height) > MAX_EDGE) {
    throw new Error("attachment_too_large");
  }
  const scale = Math.min(1, THUMB_EDGE / Math.max(header.width, header.height));
  const width = Math.max(1, Math.round(header.width * scale));
  const height = Math.max(1, Math.round(header.height * scale));
  if (header.format === "gif") {
    const reader = new GifReader(bytes);
    const rgba = new Uint8Array(reader.width * reader.height * 4);
    reader.decodeAndBlitFrameRGBA(0, rgba);
    return Transformer.fromRgbaPixels(rgba, reader.width, reader.height).resize(width, height).webp(80);
  }
  return new Transformer(bytes).rotate().resize(width, height).webp(80);
}

async function handle(message) {
  const { id, sourcePath, thumbPath } = message ?? {};
  if (typeof sourcePath !== "string" || typeof thumbPath !== "string") {
    throw new Error("invalid_job");
  }
  const bytes = fs.readFileSync(sourcePath);
  const output = await thumbnail(bytes);
  const temp = `${thumbPath}.${process.pid}.tmp`;
  fs.writeFileSync(temp, output, { flag: "w" });
  fs.renameSync(temp, thumbPath);
  return { id, ok: true };
}

const port = process.parentPort;
if (!port) {
  // UtilityProcess 外直接执行：便于本地 smoke（可选参数 source thumb）。
  const [, , source, thumb] = process.argv;
  if (source && thumb) {
    handle({ id: 0, sourcePath: source, thumbPath: thumb })
      .then(() => process.exit(0))
      .catch((error) => {
        console.error(String(error?.message ?? error));
        process.exit(1);
      });
  }
} else {
  port.on("message", (event) => {
    const message = event?.data;
    handle(message)
      .then((result) => port.postMessage(result))
      .catch((error) => port.postMessage({ id: message?.id, ok: false, error: String(error?.message ?? error) }));
  });
}
