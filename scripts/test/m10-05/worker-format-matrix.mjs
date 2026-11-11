#!/usr/bin/env node
/**
 * M10-05 真实四格式 worker 矩阵：真实 PNG/JPEG/WebP/GIF → UtilityProcess worker → WebP。
 *
 * 与 decode-spike 的区别：本脚本走**生产 worker 文件**与真实子进程（非进程内直接调用），
 * 验证 `attachmentWorker.cjs` 的 native 加载、像素前置检查与 512px 输出。
 */
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmp = path.join(root, "scripts", "test", "m10-05", ".tmp");
mkdirSync(tmp, { recursive: true });
const requireFromApp = createRequire(path.join(root, "apps", "desktop", "package.json"));
const { Transformer } = requireFromApp("@napi-rs/image");
const { GifWriter } = requireFromApp("omggif");

function crc32(buf) {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let c = 0xffffffff;
  for (const byte of buf) c = table[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([length, typeBuf, data, crc]);
}

function png(width, height) {
  const stride = width * 4;
  const raw = Buffer.allocUnsafe(height * (stride + 1));
  for (let y = 0; y < height; y += 1) {
    const row = y * (stride + 1);
    raw[row] = 0;
    for (let x = 0; x < width; x += 1) {
      const off = row + 1 + x * 4;
      raw[off] = x % 256;
      raw[off + 1] = y % 256;
      raw[off + 2] = 0x80;
      raw[off + 3] = x < 8 ? 0x00 : 0xff;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 1 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

function gif(width, height) {
  const palette = [0xff0000, 0x00ff00, 0x0000ff, 0x000000];
  const pixels = new Uint8Array(width * height);
  for (let i = 0; i < pixels.length; i += 1) pixels[i] = i % 3;
  const buf = Buffer.alloc(width * height * 2 + 1024);
  const writer = new GifWriter(buf, width, height, { palette });
  writer.addFrame(0, 0, width, height, pixels, { palette });
  writer.addFrame(0, 0, width, height, pixels, { palette });
  return buf.subarray(0, writer.end());
}

function pngHeaderOnly(width, height) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

function runWorker(source, target) {
  execFileSync(process.execPath, [path.join(root, "apps/desktop/electron/attachmentWorker.cjs"), source, target], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
  });
}

const base = png(1024, 768);
const fixtures = [
  ["png", "png", base],
  ["jpeg", "jpg", await new Transformer(base).jpeg(85)],
  ["webp", "webp", await new Transformer(base).webp(80)],
  ["gif", "gif", gif(320, 240)],
];

const results = [];
for (const [name, extension, bytes] of fixtures) {
  const source = path.join(tmp, `worker-real-${name}.${extension}`);
  const target = path.join(tmp, `worker-real-${name}.webp`);
  writeFileSync(source, bytes);
  runWorker(source, target);
  const output = readFileSync(target);
  const meta = await new Transformer(output).metadata(true);
  const ok =
    output.subarray(0, 4).toString("ascii") === "RIFF" &&
    output.subarray(8, 12).toString("ascii") === "WEBP" &&
    meta.width <= 512 &&
    meta.height <= 512 &&
    meta.width > 0 &&
    meta.height > 0;
  results.push({ format: name, input_bytes: bytes.length, output_bytes: output.length, width: meta.width, height: meta.height, ok });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}: ${bytes.length}B -> ${meta.width}x${meta.height} WebP ${output.length}B`);
}

// 25MP+1 像素：仅文件头，worker 必须在构造解码器前拒绝（退出码 1）。
{
  const source = path.join(tmp, "worker-real-overlimit.png");
  writeFileSync(source, pngHeaderOnly(5001, 5000));
  let rejected = false;
  let message = "";
  try {
    runWorker(source, path.join(tmp, "worker-real-overlimit.webp"));
  } catch (error) {
    rejected = true;
    message = `${error.stderr ?? ""}`.trim() || String(error.message);
  }
  const ok = rejected && message.includes("attachment_too_large");
  results.push({ format: "overlimit", rejected, ok, message });
  console.log(`${ok ? "PASS" : "FAIL"} overlimit: ${message}`);
}

const ok = results.every((item) => item.ok);
writeFileSync(
  path.join(tmp, "worker-format-matrix.json"),
  `${JSON.stringify({ ok, results }, null, 2)}\n`,
  "utf8",
);
if (!ok) {
  console.error("[worker-format-matrix] FAIL");
  process.exit(1);
}
console.log(`[worker-format-matrix] PASS：${results.length}/${results.length}`);
