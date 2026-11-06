/**
 * Q3 打包 spike：Electron Node 运行时 N-API 兼容性检查。
 *
 * 用途：在 `ELECTRON_RUN_AS_NODE=1` 下从 `app.asar.unpacked/node_modules`
 * 加载 `@napi-rs/image`，验证 Electron 45 的 Node 24 ABI/asarUnpack 路径。
 * 不替代 UtilityProcess；UtilityProcess 运行时由 CI/交互式桌面执行。
 */
const { createRequire } = require("node:module");
const { existsSync, readdirSync } = require("node:fs");
const path = require("node:path");
const zlib = require("node:zlib");

function pngChunk(type, data) {
  const table = [];
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let c = 0xffffffff;
  const typeBuf = Buffer.from(type, "ascii");
  for (const byte of Buffer.concat([typeBuf, data])) c = table[(c ^ byte) & 0xff] ^ (c >>> 8);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE((c ^ 0xffffffff) >>> 0, 0);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  return Buffer.concat([length, typeBuf, data, crc]);
}

function png(width, height) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const raw = Buffer.alloc(width * height * 4 + height);
  const idat = zlib.deflateSync(raw, { level: 1 });
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", idat),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

(async () => {
  const modules = process.env.AETHER_SPIKE_UNPACKED_MODULES;
  if (!modules) throw new Error("AETHER_SPIKE_UNPACKED_MODULES missing");
  const requireFromUnpacked = createRequire(path.join(modules, "noop.cjs"));
  const { Transformer } = requireFromUnpacked("@napi-rs/image");
  const imageEntry = requireFromUnpacked.resolve("@napi-rs/image");
  const napiDir = path.dirname(path.dirname(imageEntry));
  let nativeNode = null;
  try {
    for (const name of readdirSync(napiDir)) {
      if (!name.startsWith("image-")) continue;
      const dir = path.join(napiDir, name);
      for (const file of readdirSync(dir)) {
        if (file.endsWith(".node")) nativeNode = path.join(dir, file);
      }
    }
  } catch {
    /* 结果字段呈现 null */
  }
  const input = png(32, 24);
  const output = await new Transformer(input).resize(8, 6).webp(80);
  const meta = await new Transformer(output).metadata(true);
  console.log(JSON.stringify({
    ok: output.subarray(8, 12).toString("ascii") === "WEBP" && meta.width === 8 && meta.height === 6,
    electron: process.versions.electron,
    node: process.versions.node,
    modules,
    native_entry: nativeNode,
    native_exists: nativeNode !== null && existsSync(nativeNode),
  }));
})().catch((error) => {
  console.error(error instanceof Error ? error.stack : String(error));
  process.exit(1);
});
