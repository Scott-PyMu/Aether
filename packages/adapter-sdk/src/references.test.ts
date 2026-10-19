import { describe, expect, it } from "vitest";

import {
  composePromptWithReferences,
  MAX_SESSION_REFERENCES,
  parseReferences,
  renderReferenceBlock,
} from "./references";

describe("M7-06 引用注入映射（ADR-024 附录 A-3）", () => {
  it("渲染形态逐字（起止标记 + 清单 + 指令行）", () => {
    const block = renderReferenceBlock([
      { path: "C:\\ws\\a.txt", kind: "file" },
      { path: "C:\\ws\\dir", kind: "directory" },
    ]);
    expect(block).toBe(
      [
        "<aether-references>",
        "- file: C:\\ws\\a.txt",
        "- directory: C:\\ws\\dir",
        "以上为随消息附带的文件引用，按需读取",
        "</aether-references>",
      ].join("\n"),
    );
  });

  it("无引用 → null / 原消息原样（不产生空块）", () => {
    expect(renderReferenceBlock([])).toBeNull();
    expect(composePromptWithReferences("你好", [])).toBe("你好");
  });

  it("组合提示词 = 引用块 + 空行 + 原消息", () => {
    const prompt = composePromptWithReferences("读一下文件", [
      { path: "/ws/a.txt", kind: "file" },
    ]);
    expect(prompt.startsWith("<aether-references>\n- file: /ws/a.txt\n")).toBe(true);
    expect(prompt.endsWith("</aether-references>\n\n读一下文件")).toBe(true);
  });

  it("解析宽容：未知形态忽略、超上限截断、非法 kind 剔除", () => {
    const parsed = parseReferences([
      { path: "/a", kind: "file" },
      { path: "/b", kind: "socket" },
      { path: "", kind: "file" },
      { kind: "file" },
      "x",
      { path: "/c", kind: "directory" },
    ]);
    expect(parsed).toEqual([
      { path: "/a", kind: "file" },
      { path: "/c", kind: "directory" },
    ]);
    expect(parseReferences("not-array")).toEqual([]);
    expect(parseReferences(undefined)).toEqual([]);

    const many = Array.from({ length: MAX_SESSION_REFERENCES + 5 }, (_, index) => ({
      path: `/m${index}`,
      kind: "file",
    }));
    expect(parseReferences(many).length).toBe(MAX_SESSION_REFERENCES);
  });
});
