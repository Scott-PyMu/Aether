/** 官方三运行时入口（release 构建；Mock 不进入发布包，ADR-008）。 */
import { runCli as runClaude } from "@aether/adapter-claude-code/src/cli";
import { runCli as runCodex } from "@aether/adapter-codex/src/cli";
import { runCli as runDsh } from "@aether/adapter-dsh/src/cli";

import type { HostEntries } from "./common";

export const OFFICIAL_ENTRIES: HostEntries = {
  "claude-code": { prefix: "[adapter-claude-code]", run: runClaude },
  codex: { prefix: "[adapter-codex]", run: runCodex },
  "deepseek-harness": { prefix: "[adapter-dsh]", run: runDsh },
};
