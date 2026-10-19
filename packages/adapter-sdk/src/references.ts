/**
 * 文件引用上下文注入（M7-06/ADR-024 附录 A-3）。
 *
 * D6 `session.send.references: [{ path, kind }]`（≤32 条，命令层已 canonicalize 校验）：
 * 三运行时统一映射为**消息前缀引用块**——`<aether-references>` + 路径清单 +
 * 「以上为随消息附带的文件引用，按需读取」+ `</aether-references>`。
 *
 * 边界：**核心与适配器都不读取引用文件内容**；**引用 ≠ 预授权**（Agent 读取仍经
 * `fs.read` 权限门，D9 不变）。
 */

/** 引用条目（D6 载荷形态；`kind` = `file` / `directory`）。 */
export interface SessionReference {
  path: string;
  kind: "file" | "directory";
}

/** 引用条数上限（ADR-024 §2.1-5；核心命令层同值校验）。 */
export const MAX_SESSION_REFERENCES = 32;

/** 引用块起止标记（ADR-024 A-3；冻结形态，测试断言）。 */
export const REFERENCE_BLOCK_OPEN = "<aether-references>";
export const REFERENCE_BLOCK_CLOSE = "</aether-references>";
/** 引用块内指令行（ADR-024 A-3 逐字）。 */
export const REFERENCE_BLOCK_INSTRUCTION = "以上为随消息附带的文件引用，按需读取";

/**
 * 宽容解析 `references` 入参（未知形态忽略；>32 条截断到上限——核心已校验，
 * 适配器侧保持宽容，不因协议扩展失败）。
 */
export function parseReferences(value: unknown): SessionReference[] {
  if (!Array.isArray(value)) return [];
  const references: SessionReference[] = [];
  for (const item of value) {
    if (references.length >= MAX_SESSION_REFERENCES) break;
    if (typeof item !== "object" || item === null) continue;
    const record = item as Record<string, unknown>;
    const path = record["path"];
    const kind = record["kind"];
    if (typeof path !== "string" || path.length === 0) continue;
    if (kind !== "file" && kind !== "directory") continue;
    references.push({ path, kind });
  }
  return references;
}

/**
 * 渲染消息前缀引用块；无引用 → `null`（调用方按原文本发送，不产生空块）。
 *
 * 形状（冻结）：
 * ```text
 * <aether-references>
 * - file: /abs/path/a.txt
 * - directory: /abs/path/dir
 * 以上为随消息附带的文件引用，按需读取
 * </aether-references>
 * ```
 */
export function renderReferenceBlock(references: readonly SessionReference[]): string | null {
  if (references.length === 0) return null;
  const lines = [REFERENCE_BLOCK_OPEN];
  for (const reference of references) {
    lines.push(`- ${reference.kind}: ${reference.path}`);
  }
  lines.push(REFERENCE_BLOCK_INSTRUCTION);
  lines.push(REFERENCE_BLOCK_CLOSE);
  return lines.join("\n");
}

/**
 * 组合最终提示词：引用块 + 空行 + 原消息（无引用 → 原消息原样）。
 */
export function composePromptWithReferences(
  text: string,
  references: readonly SessionReference[],
): string {
  const block = renderReferenceBlock(references);
  return block === null ? text : `${block}\n\n${text}`;
}
