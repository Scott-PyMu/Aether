import { mkdirSync, mkdtempSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import {
  materializePromptPlugin,
  promptFilePath,
  PROMPT_DIR_ENV,
  PROMPT_SECTION_NAME,
} from "./dsh-plugin";

/**
 * Q5 `subagent_context` spike（M10-02 前置门禁；ADR-032 v1.1）。
 *
 * 验证生产提示词插件注册的 section `text` 是函数，DSH 每次 assembly 都会重新
 * 调用它读取侧信道文件；因此同一 agent 内替换 `<aether:subagent-results>` 块
 * 不需要重建 agent，也不会累积旧块。
 */
describe("Q5：DSH 提示词 section 热更新", () => {
  it("同一 section 在文件 v1→v2 后返回最新内容，无需重新注册", async () => {
    const home = realpathSync.native(mkdtempSync(join(tmpdir(), "aether-q5-dsh-")));
    const promptDir = join(home, "prompts");
    mkdirSync(promptDir, { recursive: true });
    const materialized = materializePromptPlugin(join(home, "profiles", "q5"));
    const sessionId = "q5-session";
    const file = promptFilePath(promptDir, sessionId);

    const previous = process.env[PROMPT_DIR_ENV];
    process.env[PROMPT_DIR_ENV] = promptDir;
    try {
      const moduleUrl = `${pathToFileURL(materialized.indexPath).href}?v=${Date.now()}`;
      const plugin = (await import(moduleUrl)) as {
        apply: (ctx: unknown) => void;
      };
      const handlers: Array<(payload: { agent: unknown }) => void> = [];
      const ctx = {
        on: (event: string, listener: (payload: { agent: unknown }) => void) => {
          if (event === "agent/created") handlers.push(listener);
          return () => undefined;
        },
      };
      plugin.apply(ctx);
      expect(handlers).toHaveLength(1);

      const sections: Array<{ name: string; text: () => string }> = [];
      const agent = {
        id: sessionId,
        ctx: {
          systemPrompt: {
            section: (section: { name: string; text: () => string }) => {
              sections.push(section);
              return () => undefined;
            },
          },
        },
      };

      writeFileSync(file, '<aether:subagent-results version="1">TAG1</aether:subagent-results>\n', "utf8");
      handlers[0]?.({ agent });
      expect(sections).toHaveLength(1);
      expect(sections[0]?.name).toBe(PROMPT_SECTION_NAME);
      expect(sections[0]?.text()).toContain("TAG1");

      writeFileSync(file, '<aether:subagent-results version="2">TAG2</aether:subagent-results>\n', "utf8");
      // 不重新注册 section；模拟下一次 assembly 再次调用 text()。
      expect(sections).toHaveLength(1);
      expect(sections[0]?.text()).toContain("TAG2");
      expect(sections[0]?.text()).not.toContain("TAG1");
    } finally {
      if (previous === undefined) delete process.env[PROMPT_DIR_ENV];
      else process.env[PROMPT_DIR_ENV] = previous;
    }
  });
});
