/**
 * 帧层工具（D6：LF 分隔、单帧硬上限 2MiB、`artifact_ref` 引用帧 <1MiB）。
 *
 * 与 Rust 侧 `AetherLineCodec` 同语义：编码时拒绝 CR/LF 与超限（防帧注入），
 * 1–2MiB 声称 `artifact_ref` 的帧按契约违约拒绝；读取时按 LF 分割并容忍 CRLF。
 */

import { ARTIFACT_REF_LIMIT_BYTES, ARTIFACT_REF_TYPE, MAX_FRAME_BYTES } from "./protocol";

export class FramingError extends Error {
  constructor(
    message: string,
    readonly kind: "invalid-outbound" | "line-too-long" | "invalid-utf8" | "artifact-ref-contract",
  ) {
    super(message);
    this.name = "FramingError";
  }
}

/**
 * 行是否声称顶层 `artifact_ref`（保守判别：先严格 JSON 解析，失败时回退子串匹配）。
 *
 * 只在行达到 1MiB 契约上限后才调用，用于识别 1–2MiB 的契约违约帧。
 */
function claimsArtifactRef(line: string): boolean {
  try {
    const parsed = JSON.parse(line) as { type?: unknown };
    return parsed?.type === ARTIFACT_REF_TYPE;
  } catch {
    return new RegExp(`"type"\\s*:\\s*"${ARTIFACT_REF_TYPE}"`).test(line);
  }
}

function checkFrameSize(line: string): void {
  const bytes = Buffer.byteLength(line, "utf8");
  if (bytes > MAX_FRAME_BYTES) {
    throw new FramingError(`帧超过 ${MAX_FRAME_BYTES} 字节上限（2MiB）`, "line-too-long");
  }
  if (bytes >= ARTIFACT_REF_LIMIT_BYTES && claimsArtifactRef(line)) {
    throw new FramingError(
      `artifact_ref 引用帧越界（${bytes} 字节）：引用帧必须 <${ARTIFACT_REF_LIMIT_BYTES} 字节`,
      "artifact-ref-contract",
    );
  }
}

/** 编码一行（含 LF）；拒绝内嵌换行、超过 2MiB 的行与越界引用帧。 */
export function encodeLine(line: string): string {
  if (line.includes("\n") || line.includes("\r")) {
    throw new FramingError("出站帧不允许包含 CR/LF（防帧注入）", "invalid-outbound");
  }
  checkFrameSize(line);
  return `${line}\n`;
}

/** 增量行读取器：把字节/文本块切分为完整行，超限/违约即抛错。 */
export class LineReader {
  private readonly decoder = new TextDecoder("utf-8", { fatal: true });
  private buffer = "";

  push(chunk: Uint8Array | string): string[] {
    let text: string;
    if (typeof chunk === "string") {
      text = chunk;
    } else {
      try {
        text = this.decoder.decode(chunk, { stream: true });
      } catch {
        throw new FramingError("帧不是合法 UTF-8", "invalid-utf8");
      }
    }
    this.buffer += text;
    const lines: string[] = [];
    let index = this.buffer.indexOf("\n");
    while (index >= 0) {
      let line = this.buffer.slice(0, index);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      this.buffer = this.buffer.slice(index + 1);
      if (line.trim().length > 0) {
        checkFrameSize(line);
        lines.push(line);
      }
      index = this.buffer.indexOf("\n");
    }
    checkFrameSize(this.buffer);
    return lines;
  }

  /** 残行（EOF 时的半行/断流）。 */
  flush(): string | undefined {
    const rest = this.buffer;
    this.buffer = "";
    return rest.length > 0 ? rest : undefined;
  }
}
