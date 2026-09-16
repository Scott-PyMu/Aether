/**
 * ULID 生成（D4：事件唯一 ID）。
 *
 * 26 字符 Crockford Base32：10 字符时间戳（48 bit）+ 16 字符随机（80 bit）。
 * 同一毫秒内单调递增，保证事件 id 可排序且不重复。
 */

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

let lastTime = 0;
const lastRandom = new Uint8Array(10);

function fillRandom(): void {
  const cryptoObj = globalThis.crypto;
  if (cryptoObj && typeof cryptoObj.getRandomValues === "function") {
    cryptoObj.getRandomValues(lastRandom);
    return;
  }
  for (let index = 0; index < lastRandom.length; index += 1) {
    lastRandom[index] = Math.floor(Math.random() * 256);
  }
}

function incrementRandom(): void {
  for (let index = lastRandom.length - 1; index >= 0; index -= 1) {
    const current = lastRandom[index] ?? 0;
    if (current < 0xff) {
      lastRandom[index] = current + 1;
      return;
    }
    lastRandom[index] = 0;
  }
}

/** 生成 ULID；`now` 可注入以便测试。 */
export function ulid(now: number = Date.now()): string {
  if (now > lastTime) {
    lastTime = now;
    fillRandom();
  } else {
    incrementRandom();
  }

  let time = BigInt(lastTime);
  let timeChars = "";
  for (let index = 0; index < 10; index += 1) {
    timeChars = CROCKFORD[Number(time & 31n)] + timeChars;
    time >>= 5n;
  }

  let random = 0n;
  for (const byte of lastRandom) {
    random = (random << 8n) | BigInt(byte);
  }
  let randomChars = "";
  for (let index = 0; index < 16; index += 1) {
    randomChars = CROCKFORD[Number(random & 31n)] + randomChars;
    random >>= 5n;
  }

  return timeChars + randomChars;
}

/** 仅测试用：重置单调状态。 */
export function resetUlidState(): void {
  lastTime = 0;
  lastRandom.fill(0);
}

export function isUlid(value: string): boolean {
  if (value.length !== 26) return false;
  for (const char of value) {
    if (!CROCKFORD.includes(char)) return false;
  }
  return true;
}
