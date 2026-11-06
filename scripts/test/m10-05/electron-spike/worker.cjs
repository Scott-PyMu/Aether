/**
 * Q3 解码 spike：UtilityProcess worker。
 *
 * 在受限 UtilityProcess（非沙箱）中加载 `@napi-rs/image` + `omggif`，
 * 生成四格式夹具并完成 512px WebP 缩略图；返回值包含模块解析路径。
 */
const { createRequire } = require("node:module");
const { existsSync, readdirSync } = require("node:fs");
const path = require("node:path");
const zlib = require("node:zlib");

const MAX_PIXELS = 25_000_000;
const MAX_EDGE = 8192;

function pngChunk(type, data) {
  let crcTable = [];
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crcTable[n] = c >>> 0;
  }
  const crc32 = (buf) => {
    let c = 0xffffffff;
    for (const byte of buf) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([length, typeBuf, data, crc]);
}

function encodePng(width, height, pixel = () => [0x20, 0x80, 0xc0, 0xff]) {
  const stride = width * 4;
  const raw = Buffer.allocUnsafe(height * (stride + 1));
  for (let y = 0; y < height; y += 1) {
    const row = y * (stride + 1);
    for (let x = 0; x < width; x += 1) {
      const [r, g, b, a] = pixel(x, y);
      const off = row + 1 + x * 4;
      raw[off] = r;
      raw[off + 1] = g;
      raw[off + 2] = b;
      raw[off + 3] = a;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", zlib.deflateSync(raw, { level: 1 })),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

function pngHeaderOnly(width, height) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

function readImageHeader(buf) {
  if (buf.length >= 24 && buf[0] === 0x89 && buf.toString("ascii", 1, 4) === "PNG") {
    return { format: "png", width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
  }
  if (buf.length >= 30 && buf.toString("ascii", 0, 4) === "RIFF" && buf.toString("ascii", 8, 12) === "WEBP") {
    const chunk = buf.toString("ascii", 12, 16);
    if (chunk === "VP8X") return { format: "webp", width: buf.readUIntLE(24, 3) + 1, height: buf.readUIntLE(27, 3) + 1 };
    if (chunk === "VP8 ") return { format: "webp", width: buf.readUInt16LE(26) & 0x3fff, height: buf.readUInt16LE(28) & 0x3fff };
    if (chunk === "VP8L" && buf[20] === 0x2f) {
      const b = buf.subarray(21, 25);
      return { format: "webp", width: 1 + (((b[1] & 0x3f) << 8) | b[0]), height: 1 + (((b[3] & 0x0f) << 10) | (b[2] << 2) | (b[1] >> 6)) };
    }
    return { format: "webp", width: null, height: null };
  }
  if (buf.length >= 10 && (buf.toString("ascii", 0, 6) === "GIF87a" || buf.toString("ascii", 0, 6) === "GIF89a")) {
    return { format: "gif", width: buf.readUInt16LE(6), height: buf.readUInt16LE(8) };
  }
  if (buf.length >= 4 && buf[0] === 0xff && buf[1] === 0xd8) {
    let offset = 2;
    while (offset + 4 <= buf.length) {
      if (buf[offset] !== 0xff) {
        offset += 1;
        continue;
      }
      const marker = buf[offset + 1];
      if (marker === 0xd8 || marker === 0xd9 || marker === 0x01 || (marker >= 0xd0 && marker <= 0xd7)) {
        offset += 2;
        continue;
      }
      const length = buf.readUInt16BE(offset + 2);
      if ([0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker)) {
        return { format: "jpeg", height: buf.readUInt16BE(offset + 5), width: buf.readUInt16BE(offset + 7) };
      }
      offset += 2 + length;
    }
    return { format: "jpeg", width: null, height: null };
  }
  return { format: "unknown", width: null, height: null };
}

function allowed(header) {
  if (header.width === null) return false;
  return header.width * header.height <= MAX_PIXELS && Math.max(header.width, header.height) <= MAX_EDGE;
}

/** 在 `node_modules/@napi-rs/image-*` 中查找当前平台的 `.node`（跨平台）。 */
function findNativeNode(imageEntry) {
  const napiDir = path.dirname(path.dirname(imageEntry));
  try {
    for (const name of readdirSync(napiDir)) {
      if (!name.startsWith("image-")) continue;
      const dir = path.join(napiDir, name);
      for (const file of readdirSync(dir)) {
        if (file.endsWith(".node")) return { package: name, path: path.join(dir, file) };
      }
    }
  } catch {
    /* 打包布局差异由结果字段呈现 */
  }
  return null;
}

async function run() {
  const appRoot = process.env.AETHER_SPIKE_APP_ROOT || process.cwd();
  const resources = process.env.AETHER_SPIKE_RESOURCES || process.cwd();
  const requireFromApp = createRequire(path.join(appRoot, "package.json"));
  const { Transformer } = requireFromApp("@napi-rs/image");
  const { GifReader, GifWriter } = requireFromApp("omggif");

  const png = encodePng(64, 48, (x, y) => [x * 4, y * 5, 0x80, x < 8 ? 0 : 255]);
  const jpeg = await new Transformer(png).jpeg(85);
  const webp = await new Transformer(png).webp(80);

  const palette = [0xff0000, 0x00ff00, 0x0000ff, 0x000000];
  const pixels = new Uint8Array(64 * 48);
  for (let i = 0; i < pixels.length; i += 1) pixels[i] = i % 3;
  const gifBuf = Buffer.alloc(64 * 48 * 2 + 1024);
  const writer = new GifWriter(gifBuf, 64, 48, { palette });
  writer.addFrame(0, 0, 64, 48, pixels, { palette });
  const gif = gifBuf.subarray(0, writer.end());

  const checks = [];
  for (const [name, bytes] of [["PNG", png], ["JPEG", jpeg], ["WebP", webp], ["GIF", gif]]) {
    const header = readImageHeader(bytes);
    let out;
    if (header.format === "gif") {
      const reader = new GifReader(bytes);
      const rgba = new Uint8Array(reader.width * reader.height * 4);
      reader.decodeAndBlitFrameRGBA(0, rgba);
      out = await Transformer.fromRgbaPixels(rgba, reader.width, reader.height).resize(64, 48).webp(80);
    } else {
      out = await new Transformer(bytes).rotate().resize(64, 48).webp(80);
    }
    checks.push({ name: `${name} → WebP`, ok: out.subarray(8, 12).toString("ascii") === "WEBP", bytes: out.length });
  }
  checks.push({ name: "25MP 上限文件头允许", ok: allowed(readImageHeader(pngHeaderOnly(5000, 5000))) });
  checks.push({ name: "25MP+1 文件头拒绝", ok: !allowed(readImageHeader(pngHeaderOnly(5001, 5000))) });
  checks.push({ name: "8193 最长边拒绝", ok: !allowed(readImageHeader(pngHeaderOnly(8193, 100))) });

  const imageEntry = requireFromApp.resolve("@napi-rs/image");
  const native = findNativeNode(imageEntry);

  return {
    checks,
    ok: checks.every((item) => item.ok),
    electron: process.versions.electron || null,
    node: process.versions.node,
    app_root: appRoot,
    resources,
    app_root_is_asar: appRoot.includes("app.asar"),
    image_entry: imageEntry,
    native_package: native?.package ?? null,
    native_node_path: native?.path ?? null,
    native_node_exists: native !== null && existsSync(native.path),
    is_packaged: process.env.AETHER_SPIKE_IS_PACKAGED === "1",
  };
}

process.parentPort.on("message", () => {
  run().then(
    (result) => process.parentPort.postMessage(result),
    (error) => process.parentPort.postMessage({ ok: false, error: error instanceof Error ? error.message : String(error) }),
  );
});
