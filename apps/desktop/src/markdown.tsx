/**
 * 安全 Markdown 子集渲染（M3-02；消息流 Markdown + 代码高亮）。
 *
 * 口径：
 * - **不产生原始 HTML**：全部输出 React 元素，不使用 `dangerouslySetInnerHTML`，
 *   从根上规避消息内容注入（CSP 之外的第二道防线，D7 安全基线）；
 * - 支持子集：标题 / 段落 / 无序与有序列表 / 引用 / 围栏代码块 / 行内代码 /
 *   粗体 / 斜体 / 链接（仅 `http(s)`，其余按纯文本渲染）；
 * - 代码高亮：内置轻量分词器（注释 / 字符串 / 数字 / 关键字），覆盖
 *   js/ts/json/rust/python/bash 关键字表；输出带 `data-token` 的 `<span>`
 *   供测试与样式断言。
 */
import type { ReactNode } from "react";

const KEYWORDS: Record<string, string[]> = {
  javascript: [
    "const", "let", "var", "function", "return", "if", "else", "for", "while",
    "import", "export", "from", "class", "new", "await", "async", "try", "catch",
    "throw", "typeof", "of", "in", "null", "undefined", "true", "false",
  ],
  typescript: [
    "const", "let", "var", "function", "return", "if", "else", "for", "while",
    "import", "export", "from", "class", "new", "await", "async", "try", "catch",
    "throw", "typeof", "of", "in", "null", "undefined", "true", "false",
    "interface", "type", "enum", "extends", "implements", "public", "private",
    "readonly", "as", "satisfies",
  ],
  rust: [
    "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "match", "if",
    "else", "for", "while", "loop", "return", "use", "mod", "crate", "self",
    "Self", "async", "await", "move", "where", "dyn", "true", "false", "None",
    "Some", "Ok", "Err",
  ],
  python: [
    "def", "class", "return", "if", "elif", "else", "for", "while", "import",
    "from", "as", "with", "try", "except", "raise", "lambda", "None", "True",
    "False", "and", "or", "not", "in", "is",
  ],
  bash: [
    "if", "then", "else", "fi", "for", "do", "done", "while", "case", "esac",
    "function", "echo", "export", "local", "return", "exit",
  ],
  json: ["true", "false", "null"],
};

function keywordsFor(language: string): string[] {
  const normalized = language.trim().toLowerCase();
  const key = normalized === "js"
    ? "javascript"
    : normalized === "ts" || normalized === "tsx"
      ? "typescript"
      : normalized === "jsx"
        ? "javascript"
        : normalized === "rs"
          ? "rust"
          : normalized === "py"
            ? "python"
            : normalized === "sh" || normalized === "shell"
              ? "bash"
              : normalized;
  return KEYWORDS[key] ?? [];
}

interface CodeToken {
  kind: "plain" | "comment" | "string" | "number" | "keyword";
  text: string;
}

/** 轻量代码分词（注释 / 字符串 / 数字 / 关键字；保持原文本顺序）。 */
export function tokenizeCode(code: string, language: string): CodeToken[] {
  const keywords = new Set(keywordsFor(language));
  const tokens: CodeToken[] = [];
  let plain = "";
  let index = 0;

  const flushPlain = () => {
    if (plain.length > 0) {
      tokens.push({ kind: "plain", text: plain });
      plain = "";
    }
  };

  while (index < code.length) {
    const rest = code.slice(index);
    const char = code[index] ?? "";

    // 行注释（`//` 与 `#`；bash/python 以 # 开头）。
    if (rest.startsWith("//") || (char === "#" && language !== "json")) {
      flushPlain();
      const end = rest.indexOf("\n");
      const text = end >= 0 ? rest.slice(0, end) : rest;
      tokens.push({ kind: "comment", text });
      index += text.length;
      continue;
    }
    // 块注释。
    if (rest.startsWith("/*")) {
      flushPlain();
      const end = rest.indexOf("*/");
      const text = end >= 0 ? rest.slice(0, end + 2) : rest;
      tokens.push({ kind: "comment", text });
      index += text.length;
      continue;
    }
    // 字符串。
    if (char === '"' || char === "'" || char === "`") {
      flushPlain();
      let cursor = index + 1;
      while (cursor < code.length) {
        const current = code[cursor];
        if (current === "\\") {
          cursor += 2;
          continue;
        }
        if (current === char) {
          cursor += 1;
          break;
        }
        cursor += 1;
      }
      const text = code.slice(index, cursor);
      tokens.push({ kind: "string", text });
      index = cursor;
      continue;
    }
    // 数字。
    if (/[0-9]/.test(char)) {
      flushPlain();
      let cursor = index;
      while (cursor < code.length && /[0-9a-fA-FxX._]/.test(code[cursor] ?? "")) {
        cursor += 1;
      }
      tokens.push({ kind: "number", text: code.slice(index, cursor) });
      index = cursor;
      continue;
    }
    // 标识符 / 关键字。
    if (/[A-Za-z_$]/.test(char)) {
      let cursor = index;
      while (cursor < code.length && /[A-Za-z0-9_$]/.test(code[cursor] ?? "")) {
        cursor += 1;
      }
      const word = code.slice(index, cursor);
      if (keywords.has(word)) {
        flushPlain();
        tokens.push({ kind: "keyword", text: word });
      } else {
        plain += word;
      }
      index = cursor;
      continue;
    }
    plain += char;
    index += 1;
  }
  flushPlain();
  return tokens;
}

function renderCode(code: string, language: string, key: string): ReactNode {
  return (
    <pre key={key} className="md-code-block" data-testid="markdown-code">
      <code data-language={language}>
        {tokenizeCode(code, language).map((token, tokenIndex) =>
          token.kind === "plain" ? (
            <span key={tokenIndex}>{token.text}</span>
          ) : (
            <span key={tokenIndex} data-token={token.kind}>
              {token.text}
            </span>
          ),
        )}
      </code>
    </pre>
  );
}

const INLINE_PATTERN =
  /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|(\[[^\]]+\]\([^)]+\))/g;

function renderInline(text: string, keyPrefix: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  let cursor = 0;
  let match: RegExpExecArray | null;
  INLINE_PATTERN.lastIndex = 0;
  while ((match = INLINE_PATTERN.exec(text)) !== null) {
    if (match.index > cursor) {
      nodes.push(text.slice(cursor, match.index));
    }
    const token = match[0];
    const key = `${keyPrefix}-${match.index}`;
    if (token.startsWith("`")) {
      nodes.push(
        <code key={key} className="md-inline-code">
          {token.slice(1, -1)}
        </code>,
      );
    } else if (token.startsWith("**")) {
      nodes.push(<strong key={key}>{token.slice(2, -2)}</strong>);
    } else if (token.startsWith("*")) {
      nodes.push(<em key={key}>{token.slice(1, -1)}</em>);
    } else {
      const linkMatch = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(token);
      const label = linkMatch?.[1] ?? token;
      const href = linkMatch?.[2] ?? "";
      if (/^https?:\/\//.test(href)) {
        nodes.push(
          <a key={key} href={href} target="_blank" rel="noreferrer">
            {label}
          </a>,
        );
      } else {
        nodes.push(label);
      }
    }
    cursor = match.index + token.length;
  }
  if (cursor < text.length) {
    nodes.push(text.slice(cursor));
  }
  return nodes;
}

/** 安全 Markdown 子集渲染（块级 + 行内）。 */
export function Markdown({ text }: { text: string }): JSX.Element {
  const lines = text.split("\n");
  const blocks: ReactNode[] = [];
  let index = 0;
  let key = 0;

  while (index < lines.length) {
    const line = lines[index] ?? "";

    if (line.trim().startsWith("```")) {
      const language = line.trim().slice(3).trim();
      const codeLines: string[] = [];
      index += 1;
      while (index < lines.length && !(lines[index] ?? "").trim().startsWith("```")) {
        codeLines.push(lines[index] ?? "");
        index += 1;
      }
      index += 1; // 跳过闭合围栏
      blocks.push(renderCode(codeLines.join("\n"), language, `code-${key}`));
      key += 1;
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      const level = Math.min(heading[1]?.length ?? 1, 6);
      const Tag = `h${level}` as keyof JSX.IntrinsicElements;
      blocks.push(
        <Tag key={`heading-${key}`} className="md-heading" data-level={level}>
          {renderInline(heading[2] ?? "", `heading-${key}`)}
        </Tag>,
      );
      key += 1;
      index += 1;
      continue;
    }

    if (/^>\s?/.test(line)) {
      const quoteLines: string[] = [];
      while (index < lines.length && /^>\s?/.test(lines[index] ?? "")) {
        quoteLines.push((lines[index] ?? "").replace(/^>\s?/, ""));
        index += 1;
      }
      blocks.push(
        <blockquote key={`quote-${key}`} className="md-quote">
          {renderInline(quoteLines.join(" "), `quote-${key}`)}
        </blockquote>,
      );
      key += 1;
      continue;
    }

    if (/^[-*]\s+/.test(line)) {
      const items: string[] = [];
      while (index < lines.length && /^[-*]\s+/.test(lines[index] ?? "")) {
        items.push((lines[index] ?? "").replace(/^[-*]\s+/, ""));
        index += 1;
      }
      blocks.push(
        <ul key={`ul-${key}`} className="md-list">
          {items.map((item, itemIndex) => (
            <li key={itemIndex}>{renderInline(item, `ul-${key}-${itemIndex}`)}</li>
          ))}
        </ul>,
      );
      key += 1;
      continue;
    }

    if (/^\d+\.\s+/.test(line)) {
      const items: string[] = [];
      while (index < lines.length && /^\d+\.\s+/.test(lines[index] ?? "")) {
        items.push((lines[index] ?? "").replace(/^\d+\.\s+/, ""));
        index += 1;
      }
      blocks.push(
        <ol key={`ol-${key}`} className="md-list">
          {items.map((item, itemIndex) => (
            <li key={itemIndex}>{renderInline(item, `ol-${key}-${itemIndex}`)}</li>
          ))}
        </ol>,
      );
      key += 1;
      continue;
    }

    if (line.trim().length === 0) {
      index += 1;
      continue;
    }

    const paragraph: string[] = [];
    while (
      index < lines.length &&
      (lines[index] ?? "").trim().length > 0 &&
      !/^(#{1,6})\s+/.test(lines[index] ?? "") &&
      !/^>\s?/.test(lines[index] ?? "") &&
      !/^[-*]\s+/.test(lines[index] ?? "") &&
      !/^\d+\.\s+/.test(lines[index] ?? "") &&
      !(lines[index] ?? "").trim().startsWith("```")
    ) {
      paragraph.push(lines[index] ?? "");
      index += 1;
    }
    blocks.push(
      <p key={`p-${key}`} className="md-paragraph">
        {renderInline(paragraph.join(" "), `p-${key}`)}
      </p>,
    );
    key += 1;
  }

  return <div className="md-body">{blocks}</div>;
}
