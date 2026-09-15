/**
 * 生成应用图标源文件 assets/app-icon.png（1024×1024，RGBA）。
 *
 * 用法：node scripts/ci/generate-app-icon.mjs [输出路径]
 * 生成后由 `tauri icon` 派生出各平台图标（Windows .ico / macOS .icns / PNG 集）。
 * 仅使用 Node 标准库（zlib），保证可重复构建。
 */
import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const SIZE = 1024;
const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const output = process.argv[2] ?? path.join(repoRoot, "assets", "app-icon.png");

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const typeAndData = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typeAndData), 0);
  return Buffer.concat([length, typeAndData, crc]);
}

function roundedRectAlpha(x, y, size, radius, inset) {
  const min = inset;
  const max = size - inset;
  const dx = Math.max(min + radius - x, 0, x - (max - radius));
  const dy = Math.max(min + radius - y, 0, y - (max - radius));
  const distance = Math.hypot(dx, dy) - radius;
  return Math.min(Math.max(0.5 - distance, 0), 1);
}

function starAlpha(x, y, size, radius) {
  const cx = size / 2;
  const cy = size / 2;
  const dx = Math.abs(x - cx) / radius;
  const dy = Math.abs(y - cy) / radius;
  const value = Math.sqrt(dx) + Math.sqrt(dy);
  const distance = (value - 1) * 70;
  return Math.min(Math.max(0.5 - distance, 0), 1);
}

function render() {
  const pixels = Buffer.alloc(SIZE * SIZE * 4);
  for (let y = 0; y < SIZE; y += 1) {
    for (let x = 0; x < SIZE; x += 1) {
      const offset = (y * SIZE + x) * 4;
      const backgroundAlpha = roundedRectAlpha(x, y, SIZE, 190, 24);
      const gradient = y / SIZE;
      let r = Math.round(29 + (14 - 29) * gradient);
      let g = Math.round(43 + (22 - 43) * gradient);
      let b = Math.round(58 + (32 - 58) * gradient);
      let alpha = backgroundAlpha;

      const glow = Math.max(0, 1 - Math.hypot(x - SIZE / 2, y - SIZE / 2) / (SIZE * 0.52));
      const glowAlpha = glow * glow * 0.55 * backgroundAlpha;
      r = Math.round(r + (59 - r) * glowAlpha);
      g = Math.round(g + (130 - g) * glowAlpha);
      b = Math.round(b + (246 - b) * glowAlpha);

      const emblem = starAlpha(x, y, SIZE, SIZE * 0.31);
      if (emblem > 0) {
        const t = y / SIZE;
        const er = Math.round(139 + (59 - 139) * t);
        const eg = Math.round(233 + (130 - 233) * t);
        const eb = Math.round(255 + (246 - 255) * t);
        r = Math.round(r * (1 - emblem) + er * emblem);
        g = Math.round(g * (1 - emblem) + eg * emblem);
        b = Math.round(b * (1 - emblem) + eb * emblem);
      }

      pixels[offset] = r;
      pixels[offset + 1] = g;
      pixels[offset + 2] = b;
      pixels[offset + 3] = Math.round(alpha * 255);
    }
  }

  const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
  for (let y = 0; y < SIZE; y += 1) {
    raw[y * (SIZE * 4 + 1)] = 0;
    pixels.copy(raw, y * (SIZE * 4 + 1) + 1, y * SIZE * 4, (y + 1) * SIZE * 4);
  }

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(SIZE, 0);
  ihdr.writeUInt32BE(SIZE, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const png = render();
mkdirSync(path.dirname(output), { recursive: true });
writeFileSync(output, png);
console.log(`[icon] 已生成 ${output}（${png.length} bytes）`);
