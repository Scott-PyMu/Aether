/**
 * 运行时自签证书（M9-10 DoD-4 证书错误路径）。
 *
 * 为什么不用仓库内 PEM 夹具：AGENTS §2.9 禁止私钥进入测试夹具。这里在 E2E
 * 启动时用 `node:crypto` 生成一次性 RSA 密钥与 X.509 自签证书（DER 手工编码），
 * 只存在于内存/临时进程，不落盘。
 *
 * 仅覆盖 PoC 断言所需字段：CN、validity、basicConstraints(CA)、IP SAN 127.0.0.1。
 */
import crypto from "node:crypto";

function derLength(length) {
  if (length < 0x80) return Buffer.from([length]);
  const bytes = [];
  let value = length;
  while (value > 0) {
    bytes.unshift(value & 0xff);
    value >>= 8;
  }
  return Buffer.from([0x80 | bytes.length, ...bytes]);
}

function tlv(tag, content) {
  return Buffer.concat([Buffer.from([tag]), derLength(content.length), content]);
}

function sequence(...items) {
  return tlv(0x30, Buffer.concat(items));
}

function set(...items) {
  return tlv(0x31, Buffer.concat(items));
}

function integer(value) {
  let hex = value.toString(16);
  if (hex.length % 2) hex = `0${hex}`;
  let bytes = Buffer.from(hex, "hex");
  if ((bytes[0] & 0x80) !== 0) bytes = Buffer.concat([Buffer.from([0]), bytes]);
  return tlv(0x02, bytes);
}

function oid(dotted) {
  const parts = dotted.split(".").map(Number);
  const bytes = [40 * parts[0] + parts[1]];
  for (const part of parts.slice(2)) {
    const stack = [part & 0x7f];
    let value = part >> 7;
    while (value > 0) {
      stack.unshift((value & 0x7f) | 0x80);
      value >>= 7;
    }
    bytes.push(...stack);
  }
  return tlv(0x06, Buffer.from(bytes));
}

function bitString(bytes) {
  return tlv(0x03, Buffer.concat([Buffer.from([0]), bytes]));
}

function utcTime(date) {
  const pad = (value) => String(value).padStart(2, "0");
  const text = `${pad(date.getUTCFullYear() % 100)}${pad(date.getUTCMonth() + 1)}${pad(
    date.getUTCDate(),
  )}${pad(date.getUTCHours())}${pad(date.getUTCMinutes())}${pad(
    date.getUTCSeconds(),
  )}Z`;
  return tlv(0x17, Buffer.from(text, "ascii"));
}

function commonName(name) {
  return sequence(set(sequence(oid("2.5.4.3"), tlv(0x0c, Buffer.from(name, "utf8")))));
}

function extension(oidText, critical, value) {
  const parts = [oid(oidText)];
  if (critical) parts.push(tlv(0x01, Buffer.from([0xff])));
  parts.push(tlv(0x04, value));
  return sequence(...parts);
}

/**
 * 生成自签证书（内存）。
 * @returns {{cert: string, key: string, fingerprint: string}}
 */
export function createSelfSignedCertificate() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", {
    modulusLength: 2_048,
  });
  const spki = publicKey.export({ type: "spki", format: "der" });
  const signatureAlgorithm = sequence(
    oid("1.2.840.113549.1.1.11"),
    tlv(0x05, Buffer.alloc(0)),
  );
  const basicConstraints = sequence(
    tlv(0x01, Buffer.from([0xff])),
    tlv(0x02, Buffer.from([0])),
  );
  const subjectAltName = sequence(tlv(0x87, Buffer.from([127, 0, 0, 1])));
  const extensions = tlv(
    0xa3,
    sequence(
      extension("2.5.29.19", true, basicConstraints),
      extension("2.5.29.17", false, subjectAltName),
    ),
  );
  const tbs = sequence(
    tlv(0xa0, integer(2)),
    integer(crypto.randomBytes(8).readUIntBE(0, 6)),
    signatureAlgorithm,
    commonName("127.0.0.1"),
    sequence(
      utcTime(new Date(Date.now() - 86_400_000)),
      utcTime(new Date(Date.now() + 86_400_000)),
    ),
    commonName("127.0.0.1"),
    spki,
    extensions,
  );
  const signature = crypto.sign("sha256", tbs, privateKey);
  const der = sequence(tbs, signatureAlgorithm, bitString(signature));
  const pem = `-----BEGIN CERTIFICATE-----\n${der
    .toString("base64")
    .match(/.{1,64}/g)
    .join("\n")}\n-----END CERTIFICATE-----\n`;
  const key = privateKey.export({ type: "pkcs8", format: "pem" }).toString();
  const fingerprint = new crypto.X509Certificate(pem).fingerprint256;
  return { cert: pem, key, fingerprint };
}
