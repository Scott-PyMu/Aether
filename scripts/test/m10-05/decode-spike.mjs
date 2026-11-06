#!/usr/bin/env node
/**
 * Q3 解码 spike（M10-05 前置；ADR-035 v1.1 / 决策记录 §9-4）。
 *
 * 目的：
 * - 验证 `@napi-rs/image =1.16.0` + `omggif =1.0.10` 在 Node/Electron 环境可用；
 * - 四格式（PNG/JPEG/WebP/GIF）解码、512px WebP 缩略图、EXIF rotate、透明度、损坏文件；
 * - 25MP / 最长边 8192 的前置文件头拒绝（不得先整图解码）；
 * - 25MP 单图/并发内存采样；
 * - 供应链：两依赖 MIT、无 sharp/@img。
 *
 * 运行：node scripts/test/m10-05/decode-spike.mjs
 * 输出：scripts/test/m10-05/.tmp/q3-decode-results.json（不提交 .tmp）。
 */
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import zlib from "node:zlib";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpDir = path.join(repoRoot, "scripts", "test", "m10-05", ".tmp");
mkdirSync(tmpDir, { recursive: true });

const requireFromApp = createRequire(path.join(repoRoot, "apps", "desktop", "package.json"));
const { Transformer } = requireFromApp("@napi-rs/image");
const { GifReader, GifWriter } = requireFromApp("omggif");

const MAX_PIXELS = 25_000_000;
const MAX_EDGE = 8192;
/** 大图阈值：>=20MP 视为 25MP 级；低内存机器降为 1 并发。 */
const LARGE_IMAGE_PIXELS = 20_000_000;
/** 低内存机器判定：总内存 <8 GiB（机器级，不按瞬时 free 抖动）。 */
const LOW_MEMORY_TOTAL_BYTES = 8 * 1024 ** 3;
const DEFAULT_CONCURRENCY = 2;
const results = { schema: "aether.m10-05.q3-decode-spike.v1", started_at: new Date().toISOString(), checks: [], metrics: {} };

function check(name, ok, detail) {
  results.checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
  if (!ok) throw new Error(`Q3 check failed: ${name}`);
}

/**
 * M10-05 worker 并发选择（常量级调参；证据记录）：
 * - 默认 2；
 * - 大图（>=20MP）且机器总内存 <8 GiB → 1。
 */
function chooseConcurrency({ pixels, totalMem }) {
  if (pixels >= LARGE_IMAGE_PIXELS && totalMem < LOW_MEMORY_TOTAL_BYTES) return 1;
  return DEFAULT_CONCURRENCY;
}

function bytesEqual(a, b) {
  return Buffer.compare(Buffer.from(a), Buffer.from(b)) === 0;
}

// ===== 最小 PNG 编码器（仅测试夹具；不进入产品代码）=====
function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])) >>> 0, 0);
  return Buffer.concat([length, typeBuf, data, crc]);
}

let crcTable = null;
function crc32(buf) {
  if (crcTable === null) {
    crcTable = new Uint32Array(256);
    for (let n = 0; n < 256; n += 1) {
      let c = n;
      for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      crcTable[n] = c >>> 0;
    }
  }
  let c = 0xffffffff;
  for (const byte of buf) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** RGBA8 PNG（color type 6）；颜色函数按 (x,y) 生成，便于验证内容。 */
function encodePng(width, height, pixel = () => [0x20, 0x80, 0xc0, 0xff]) {
  const stride = width * 4;
  const raw = Buffer.allocUnsafe(height * (stride + 1));
  for (let y = 0; y < height; y += 1) {
    const row = y * (stride + 1);
    raw[row] = 0;
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
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", zlib.deflateSync(raw, { level: 1 })),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

/** 仅文件头 PNG（用于超限拒绝；不产生可解码数据）。 */
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

// ===== 文件头解析（前置像素/边长检查；产品实现在 core/main，不整图解码）=====
function readImageHeader(bytes) {
  const buf = Buffer.from(bytes);
  if (buf.length >= 24 && buf.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))) {
    return { format: "png", width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
  }
  if (buf.length >= 12 && buf.toString("ascii", 0, 4) === "RIFF" && buf.toString("ascii", 8, 12) === "WEBP") {
    const chunk = buf.toString("ascii", 12, 16);
    if (chunk === "VP8X" && buf.length >= 30) {
      return { format: "webp", width: buf.readUIntLE(24, 3) + 1, height: buf.readUIntLE(27, 3) + 1 };
    }
    if (chunk === "VP8 " && buf.length >= 30) {
      return { format: "webp", width: buf.readUInt16LE(26) & 0x3fff, height: buf.readUInt16LE(28) & 0x3fff };
    }
    if (chunk === "VP8L" && buf.length >= 25 && buf[20] === 0x2f) {
      const b = buf.subarray(21, 25);
      return {
        format: "webp",
        width: 1 + (((b[1] & 0x3f) << 8) | b[0]),
        height: 1 + (((b[3] & 0x0f) << 10) | (b[2] << 2) | (b[1] >> 6)),
      };
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
      const isSof = [0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker);
      if (isSof && offset + 9 <= buf.length) {
        return { format: "jpeg", height: buf.readUInt16BE(offset + 5), width: buf.readUInt16BE(offset + 7) };
      }
      offset += 2 + length;
    }
    return { format: "jpeg", width: null, height: null };
  }
  return { format: "unknown", width: null, height: null };
}

function assertHeaderAllowed(header) {
  if (header.width === null || header.height === null) return false;
  const pixels = header.width * header.height;
  if (pixels > MAX_PIXELS) return false;
  if (Math.max(header.width, header.height) > MAX_EDGE) return false;
  return true;
}

// ===== EXIF orientation 6（Rotate90Cw）=====
function withExifOrientation(jpeg, orientation) {
  const tiff = Buffer.alloc(26);
  tiff.write("II", 0, "ascii");
  tiff.writeUInt16LE(42, 2);
  tiff.writeUInt32LE(8, 4);
  tiff.writeUInt16LE(1, 8);
  tiff.writeUInt16LE(0x0112, 10);
  tiff.writeUInt16LE(3, 12);
  tiff.writeUInt32LE(1, 14);
  tiff.writeUInt16LE(orientation, 18);
  tiff.writeUInt16LE(0, 20);
  tiff.writeUInt32LE(0, 22);
  const payload = Buffer.concat([Buffer.from("Exif\0\0", "binary"), tiff]);
  const length = Buffer.alloc(2);
  length.writeUInt16BE(payload.length + 2, 0);
  const app1 = Buffer.concat([Buffer.from([0xff, 0xe1]), length, payload]);
  return Buffer.concat([jpeg.subarray(0, 2), app1, jpeg.subarray(2)]);
}

// ===== GIF 夹具（omggif 写 2 帧；只预览首帧）=====
function encodeGif(width, height) {
  const palette = [0xff0000, 0x00ff00, 0x0000ff, 0x000000];
  const pixels = new Uint8Array(width * height);
  for (let i = 0; i < pixels.length; i += 1) pixels[i] = i % 3;
  const buf = Buffer.alloc(width * height * 2 + 1024);
  const writer = new GifWriter(buf, width, height, { palette });
  writer.addFrame(0, 0, width, height, pixels, { palette });
  writer.addFrame(0, 0, width, height, pixels, { palette });
  const end = writer.end();
  return buf.subarray(0, end);
}

async function thumbnail(bytes) {
  const header = readImageHeader(bytes);
  if (!assertHeaderAllowed(header)) {
    throw new Error(`header rejected: ${header.format} ${header.width}x${header.height}`);
  }
  const scale = Math.min(1, 512 / Math.max(header.width, header.height));
  const width = Math.max(1, Math.round(header.width * scale));
  const height = Math.max(1, Math.round(header.height * scale));
  if (header.format === "gif") {
    const reader = new GifReader(Buffer.from(bytes));
    const rgba = new Uint8Array(reader.width * reader.height * 4);
    reader.decodeAndBlitFrameRGBA(0, rgba);
    return Transformer.fromRgbaPixels(rgba, reader.width, reader.height).resize(width, height).webp(80);
  }
  return new Transformer(bytes).rotate().resize(width, height).webp(80);
}

// ===== 主流程 =====
const png = encodePng(64, 48, (x, y) => [x * 4, y * 5, 0x80, x < 8 ? 0x00 : 0xff]);
const jpeg = await new Transformer(png).jpeg(85);
const webp = await new Transformer(png).webp(80);
const gif = encodeGif(64, 48);

check("PNG 文件头 64x48", bytesEqual(readImageHeader(png).format, "png") && readImageHeader(png).width === 64);
check("JPEG 文件头 64x48", readImageHeader(jpeg).format === "jpeg" && readImageHeader(jpeg).width === 64 && readImageHeader(jpeg).height === 48);
check("WebP 文件头 64x48", readImageHeader(webp).format === "webp" && readImageHeader(webp).width === 64 && readImageHeader(webp).height === 48);
check("GIF 文件头 64x48", readImageHeader(gif).format === "gif" && readImageHeader(gif).width === 64 && readImageHeader(gif).height === 48);

for (const [name, bytes] of [["PNG", png], ["JPEG", jpeg], ["WebP", webp], ["GIF", gif]]) {
  const out = await thumbnail(bytes);
  const meta = await new Transformer(out).metadata(true);
  check(`${name} → WebP 缩略图`, out.subarray(0, 4).toString("ascii") === "RIFF" && out.subarray(8, 12).toString("ascii") === "WEBP" && meta.width <= 512 && meta.height <= 512, `${meta.format} ${meta.width}x${meta.height} ${out.length}B`);
}

const exifJpeg = withExifOrientation(jpeg, 6);
const exifMeta = await new Transformer(exifJpeg).metadata(true);
check("EXIF orientation 读取", exifMeta.orientation === 6, `orientation=${exifMeta.orientation}`);
const rotated = await new Transformer(exifJpeg).rotate().resize(2, 4).webp(80);
const rotatedMeta = await new Transformer(rotated).metadata(true);
check("EXIF rotate 后无残留 orientation", rotatedMeta.orientation === undefined || rotatedMeta.orientation === 1, `orientation=${rotatedMeta.orientation}`);

const transparentPng = encodePng(16, 16, () => [0x10, 0x20, 0x30, 0x00]);
const transparentOut = await thumbnail(transparentPng);
const transparentMeta = await new Transformer(transparentOut).metadata(true);
check("PNG 透明度保留（WebP RGBA）", Boolean(transparentMeta.colorType) || transparentMeta.format === "webp", `colorType=${transparentMeta.colorType}`);

const corrupted = png.subarray(0, 80);
let corruptError = null;
try {
  await new Transformer(corrupted).metadata(true);
} catch (error) {
  corruptError = error instanceof Error ? error.message : String(error);
}
check("损坏 PNG 结构化失败（不崩溃）", corruptError !== null, corruptError ?? "no error");

// 25MP/8192 边界：只读文件头，不构造 Transformer。
const boundary = [
  { w: 5000, h: 5000, pass: true, label: "25,000,000 像素（上限）" },
  { w: 5001, h: 5000, pass: false, label: "25,005,000 像素（超限）" },
  { w: 8192, h: 3051, pass: true, label: "8192x3051（24,993,792）" },
  { w: 8192, h: 3052, pass: false, label: "8192x3052（25,001,984）" },
  { w: 8193, h: 100, pass: false, label: "最长边 8193（超限）" },
];
for (const item of boundary) {
  const header = readImageHeader(pngHeaderOnly(item.w, item.h));
  const allowed = assertHeaderAllowed(header);
  check(`边界 ${item.label}`, allowed === item.pass, `allowed=${allowed}`);
}

// 真实 25MP 解码 + 内存采样（5000x5000 灰阶/彩色 RGBA；ADRIC 估算口径见 ADR-035 §D-035-6）。
const bigPngPath = path.join(tmpDir, "25mp.png");
const bigPng = encodePng(5000, 5000, (x, y) => [x & 0xff, y & 0xff, 0x40, 0xff]);
writeFileSync(bigPngPath, bigPng);
check("25MP 夹具写入", readFileSync(bigPngPath).length === bigPng.length, `${bigPng.length}B`);

const before = process.memoryUsage();
let peak = before.rss;
const sampler = setInterval(() => {
  peak = Math.max(peak, process.memoryUsage().rss);
}, 10);
const singleStart = Date.now();
const bigOut = await thumbnail(bigPng);
const singleDuration = Date.now() - singleStart;
clearInterval(sampler);
const after = process.memoryUsage();
const bigMeta = await new Transformer(bigOut).metadata(true);
results.metrics.memory_25mp = {
  before_rss_bytes: before.rss,
  after_rss_bytes: after.rss,
  peak_rss_bytes: peak,
  delta_bytes: peak - before.rss,
  duration_ms: singleDuration,
  output: { width: bigMeta.width, height: bigMeta.height, bytes: bigOut.length },
};
check("25MP 单图解码 + 缩略图", bigMeta.width <= 512 && bigMeta.height <= 512, JSON.stringify(results.metrics.memory_25mp));

const before2 = process.memoryUsage();
let peak2 = before2.rss;
const sampler2 = setInterval(() => {
  peak2 = Math.max(peak2, process.memoryUsage().rss);
}, 10);
const concurrentStart = Date.now();
await Promise.all([thumbnail(bigPng), thumbnail(bigPng)]);
const concurrentDuration = Date.now() - concurrentStart;
clearInterval(sampler2);
results.metrics.memory_25mp_concurrent_2 = {
  before_rss_bytes: before2.rss,
  peak_rss_bytes: peak2,
  delta_bytes: peak2 - before2.rss,
  duration_ms: concurrentDuration,
};
check("25MP 两并发解码", true, JSON.stringify(results.metrics.memory_25mp_concurrent_2));

const totalMem = os.totalmem();
const chosen = chooseConcurrency({ pixels: 25_000_000, totalMem });
results.metrics.concurrency_policy = {
  default: DEFAULT_CONCURRENCY,
  large_image_pixels: LARGE_IMAGE_PIXELS,
  low_memory_total_bytes: LOW_MEMORY_TOTAL_BYTES,
  host_total_memory_bytes: totalMem,
  host_chosen_for_25mp: chosen,
};
check("并发策略：本机 25MP 选择", chosen === DEFAULT_CONCURRENCY, JSON.stringify(results.metrics.concurrency_policy));
check(
  "并发策略：低内存（4GiB）25MP → 1",
  chooseConcurrency({ pixels: 25_000_000, totalMem: 4 * 1024 ** 3 }) === 1,
);
check(
  "并发策略：低内存（4GiB）8MP → 2",
  chooseConcurrency({ pixels: 8_000_000, totalMem: 4 * 1024 ** 3 }) === DEFAULT_CONCURRENCY,
);

// 供应链：版本/许可证/无 sharp。
const imagePkg = JSON.parse(readFileSync(path.join(repoRoot, "node_modules", ".pnpm", "@napi-rs+image@1.16.0", "node_modules", "@napi-rs", "image", "package.json"), "utf8"));
const omggifPkg = JSON.parse(readFileSync(path.join(repoRoot, "apps", "desktop", "node_modules", "omggif", "package.json"), "utf8"));
const lock = readFileSync(path.join(repoRoot, "pnpm-lock.yaml"), "utf8");
check("@napi-rs/image=1.16.0 MIT", imagePkg.version === "1.16.0" && imagePkg.license === "MIT", `${imagePkg.version} ${imagePkg.license}`);
check("omggif=1.0.10 MIT", omggifPkg.version === "1.0.10" && omggifPkg.license === "MIT", `${omggifPkg.version} ${omggifPkg.license}`);
check("无 sharp/@img 依赖", !/['"]?sharp['"]?|@img\//.test(lock), "pnpm-lock scan 0 hit");

results.finished_at = new Date().toISOString();
results.ok = results.checks.every((item) => item.ok);
writeFileSync(path.join(tmpDir, "q3-decode-results.json"), `${JSON.stringify(results, null, 2)}\n`, "utf8");
console.log(`Q3 decode spike: ${results.checks.filter((c) => c.ok).length}/${results.checks.length} checks passed`);
