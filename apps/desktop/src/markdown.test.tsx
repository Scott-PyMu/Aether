/**
 * Markdown 渲染单测（M3-02）：子集渲染、代码高亮、无原始 HTML 注入。
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Markdown, tokenizeCode } from "./markdown";

afterEach(cleanup);

describe("Markdown（M3-02）", () => {
  it("标题 / 粗体 / 行内代码 / 列表 / 引用渲染为语义元素", () => {
    render(
      <Markdown
        text={"# 标题\n\n这是 **粗体** 与 `code`。\n\n- 一\n- 二\n\n> 引用"}
      />,
    );
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("标题");
    expect(screen.getByText("粗体").tagName).toBe("STRONG");
    expect(screen.getByText("code").tagName).toBe("CODE");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    expect(document.querySelector("blockquote")?.textContent).toBe("引用");
  });

  it("围栏代码块带语言与高亮 token（data-token）", () => {
    render(
      <Markdown
        text={"```rust\n// 注释\nlet value = 42;\nlet text = \"hi\";\n```"}
      />,
    );
    const code = screen.getByTestId("markdown-code");
    expect(code.querySelector("code")?.getAttribute("data-language")).toBe("rust");
    const tokens = [...code.querySelectorAll("[data-token]")].map((node) =>
      node.getAttribute("data-token"),
    );
    expect(tokens).toContain("comment");
    expect(tokens).toContain("keyword");
    expect(tokens).toContain("number");
    expect(tokens).toContain("string");
  });

  it("tokenizeCode 保持原文拼接不变（不丢字符）", () => {
    const source = 'const x = "a\\"b"; // note\nfn main() { 12 }';
    const tokens = tokenizeCode(source, "typescript");
    expect(tokens.map((token) => token.text).join("")).toBe(source);
  });

  it("不产生原始 HTML：<script> 按纯文本渲染，无 script 元素", () => {
    render(<Markdown text={'<script>window.__pwned = true</script>'} />);
    expect(document.querySelector("script")).toBeNull();
    expect(
      screen.getByText("<script>window.__pwned = true</script>"),
    ).toBeTruthy();
    expect((window as unknown as { __pwned?: boolean }).__pwned).toBeUndefined();
  });

  it("链接仅放行 http(s)；javascript: 按纯文本渲染", () => {
    render(
      <Markdown
        text={
          "[安全](https://example.com) 与 [危险](javascript:alert(1))"
        }
      />,
    );
    const link = screen.getByRole("link");
    expect(link.getAttribute("href")).toBe("https://example.com");
    expect(link.getAttribute("rel")).toBe("noreferrer");
    expect(document.querySelectorAll("a")).toHaveLength(1);
    // 危险链接降级为纯文本（不生成可点击元素）。
    expect(screen.getByText(/危险/)).toBeTruthy();
    expect(screen.queryByText("危险", { selector: "a" })).toBeNull();
  });
});
