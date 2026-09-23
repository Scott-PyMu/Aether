/**
 * ULID 生成（M3-02；ADR-005 幂等键 `client_msg_id`）。
 *
 * 26 位 Crockford Base32：10 位时间戳 + 16 位随机（80 bit）。前端只用于
 * `session_send.client_msg_id`（幂等键）与本地气泡键；解析/校验在核心侧
 * （`ulid` crate，ADR-007）。
 */
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

function randomBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  const cryptoApi = globalThis.crypto;
  if (cryptoApi && typeof cryptoApi.getRandomValues === "function") {
    cryptoApi.getRandomValues(bytes);
    return bytes;
  }
  for (let index = 0; index < length; index += 1) {
    bytes[index] = Math.floor(Math.random() * 256);
  }
  return bytes;
}

/** 生成 ULID（时间戳 + 随机；同毫秒内单调不保证，幂等由核心持久化兜底）。 */
export function generateUlid(now: number = Date.now()): string {
  let time = Math.max(0, Math.floor(now));
  const timeChars = new Array<string>(10);
  for (let index = 9; index >= 0; index -= 1) {
    timeChars[index] = CROCKFORD[time % 32] ?? "0";
    time = Math.floor(time / 32);
  }
  const bytes = randomBytes(16);
  let randomChars = "";
  for (const byte of bytes) {
    randomChars += CROCKFORD[byte % 32] ?? "0";
  }
  return `${timeChars.join("")}${randomChars.slice(0, 16)}`;
}
