/**
 * DSH 增强机制（ADR-002 §3.6 / M2-11 DoD1–2）：版本门闩、插件落盘、`--patch` overlay。
 *
 * - 插件包：`<DSH_HOME>/profiles/<profile>/node_modules/aether-dsh-stream/`（独立包名；
 *   共享 `profiles/node_modules` 放置无效，见 M1-11 已知坑 19）；
 * - `package.json` 必须无 BOM（已知坑 19②：BOM 会让 `dsh-typert-loader` 直接崩溃）；
 * - overlay：覆写 `acp` 行 provider/model + `insert` 插件行（不修改 DSH 源码）。
 *
 * 合规（ADR-002 §3.5 / A11）：不拷贝任何上游代码；插件只使用 DSH 运行时公开可观察的
 * `agent/assistant-stream` 事件契约（维护风险已记录于 ADR-002/ADR-008）。
 */

import { mkdirSync, readFileSync, readdirSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

/** DSH 版本门闩（ADR-002 §3.1；M1-11 实测 ACP 只在 0.1.5+；2026-10-20 升级至 0.2.1-rc.2 并复测插件/ACP 兼容）。 */
export const DEFAULT_DSH_VERSION_PIN = "0.2.1-rc.2";
/** 插件包名（独立域；不得与 DSH 官方包同名）。 */
export const PLUGIN_PACKAGE_NAME = "aether-dsh-stream";
/** 插件契约标记（版本门闩第二条件：插件 hello 帧的 contract 字段）。 */
export const PLUGIN_CONTRACT = "aether-dsh-stream@1";
/** 插件包版本。 */
export const PLUGIN_VERSION = "0.1.0";

/** 插件 `package.json`（无 BOM；`JSON.stringify` 不产生 BOM）。 */
export const PLUGIN_PACKAGE_JSON = `${JSON.stringify(
  {
    name: PLUGIN_PACKAGE_NAME,
    version: PLUGIN_VERSION,
    type: "module",
    main: "index.js",
    private: true,
  },
  null,
  2,
)}\n`;

/**
 * 插件入口源码（自研，约 40 行；不拷贝上游/第三方代码）。
 *
 * 订阅 `agent/assistant-stream`，把原始 provider 增量以 JSONL 追加到
 * `AETHER_DSH_DELTA_FILE`（适配器注入的带外通道）；启动时写 `hello` 契约帧。
 */
export const PLUGIN_INDEX_JS = [
  'import { appendFileSync, readFileSync } from "node:fs";',
  "",
  `export const name = "${PLUGIN_PACKAGE_NAME}";`,
  "",
  "function write(frame) {",
  "  const file = process.env.AETHER_DSH_DELTA_FILE;",
  "  if (!file) return;",
  "  try {",
  '    appendFileSync(file, JSON.stringify({ v: 1, at: Date.now(), ...frame }) + "\\n");',
  "  } catch {",
  "    /* 通道失败不得影响 DSH 主流程（适配器侧按 ACP final 兜底） */",
  "  }",
  "}",
  "",
  "function dshVersion() {",
  "  try {",
  '    const pkg = JSON.parse(readFileSync(new URL("../@deepseek-ai/dsh/package.json", import.meta.url), "utf8"));',
  "    return pkg.version || null;",
  "  } catch {",
  '    return process.env.AETHER_DSH_VERSION || null;',
  "  }",
  "}",
  "",
  "export function apply(ctx) {",
  `  write({ type: "hello", contract: "${PLUGIN_CONTRACT}", dshVersion: dshVersion() });`,
  '  ctx.on("agent/assistant-stream", (payload) => {',
  "    const frame = payload && payload.frame ? payload.frame : payload;",
  "    const chunk = frame && frame.chunk ? frame.chunk : null;",
  "    const sessionId =",
  "      (payload && (payload.sessionId || payload.session_id)) ||",
  "      (frame && (frame.sessionId || frame.session_id)) ||",
  "      undefined;",
  "    write({",
  '      type: frame && frame.type ? frame.type : "chunk",',
  "      attemptId: frame && frame.attemptId,",
  "      sessionId,",
  "      chunkType: chunk && chunk.type,",
  '      text: chunk && typeof chunk.text === "string" ? chunk.text : undefined,',
  "    });",
  "  });",
  "}",
  "",
].join("\n");

export interface MaterializedPlugin {
  dir: string;
  packageJsonPath: string;
  indexPath: string;
}

/** 校验文件无 BOM（已知坑 19②；字节级断言）。 */
export function assertNoBom(path: string): void {
  const bytes = readFileSync(path);
  if (bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
    throw new Error(`插件文件含 UTF-8 BOM（DSH 加载器会崩溃）：${path}`);
  }
}

/**
 * 把插件包落盘到 profile 的 `node_modules`（幂等覆盖；无 BOM）。
 *
 * @param profileDir `<DSH_HOME>/profiles/<profile>`
 */
export function materializePlugin(profileDir: string): MaterializedPlugin {
  const dir = join(profileDir, "node_modules", PLUGIN_PACKAGE_NAME);
  mkdirSync(dir, { recursive: true });
  const packageJsonPath = join(dir, "package.json");
  const indexPath = join(dir, "index.js");
  writeFileSync(packageJsonPath, PLUGIN_PACKAGE_JSON, "utf8");
  writeFileSync(indexPath, PLUGIN_INDEX_JS, "utf8");
  assertNoBom(packageJsonPath);
  assertNoBom(indexPath);
  return { dir, packageJsonPath, indexPath };
}

// ===== M7-09/ADR-027：DSH 系统提示词注入插件 =====

/** 提示词插件包名。 */
export const PROMPT_PLUGIN_PACKAGE_NAME = "aether-dsh-prompt";
/** 提示词插件版本。 */
export const PROMPT_PLUGIN_VERSION = "0.1.0";
/** 提示词 section 名（agent 作用域；`deployment:persona-prefix` 之后、plan 之前）。 */
export const PROMPT_SECTION_NAME = "aether:session-prompt";
/** 提示词 section 排序（DSH 外部贡献可用任意有限 order；0=persona、500=plan、1000+=工具）。 */
export const PROMPT_SECTION_ORDER = 100;
/** 侧信道目录环境变量（适配器注入；插件读取 `<dir>/<encodeURIComponent(sessionId)>.md`）。 */
export const PROMPT_DIR_ENV = "AETHER_DSH_PROMPT_DIR";

/** 提示词插件 `package.json`（无 BOM）。 */
export const PROMPT_PLUGIN_PACKAGE_JSON = `${JSON.stringify(
  {
    name: PROMPT_PLUGIN_PACKAGE_NAME,
    version: PROMPT_PLUGIN_VERSION,
    type: "module",
    main: "index.js",
    private: true,
  },
  null,
  2,
)}\n`;

/**
 * 提示词插件源码（自研，约 30 行；不拷贝上游代码）。
 *
 * 设计（ADR-027）：
 * - `agent/created` 在首次提示词组装前派发；通过 `agent.ctx.systemPrompt.section`
 *   注册 **agent 作用域** section（只影响该会话，agent disposal 时自动 unwind）；
 * - `text` 为函数：每次 assemble 读取侧信道文件
 *   `<promptDir>/<encodeURIComponent(agent.id)>.md`（缺失/空 → 空串，`renderPrompt`
 *   丢弃空段）；`interpolate: false` 保留用户提示词原文；
 * - `inject = ["systemPrompt"]` 为 cordis 服务注入门控（未声明时访问服务会抛错）。
 */
export const PROMPT_PLUGIN_INDEX_JS = [
  'import { readFileSync } from "node:fs";',
  'import { join } from "node:path";',
  "",
  `export const name = "${PROMPT_PLUGIN_PACKAGE_NAME}";`,
  'export const inject = ["systemPrompt"];',
  "",
  `const PROMPT_DIR = process.env["${PROMPT_DIR_ENV}"];`,
  "",
  "function readPrompt(sessionId) {",
  "  if (!PROMPT_DIR || !sessionId) return \"\";",
  "  try {",
  "    return readFileSync(join(PROMPT_DIR, `${encodeURIComponent(sessionId)}.md`), \"utf8\");",
  "  } catch {",
  "    return \"\";",
  "  }",
  "}",
  "",
  "export function apply(ctx) {",
  "  if (!PROMPT_DIR) return;",
  '  ctx.on("agent/created", ({ agent }) => {',
  "    try {",
  "      agent.ctx.systemPrompt.section({",
  `        name: "${PROMPT_SECTION_NAME}",`,
  `        order: ${PROMPT_SECTION_ORDER},`,
  "        text: () => readPrompt(String(agent.id)),",
  "        // 用户提示词按原文注入：DSH 默认对 {{variable}} 严格插值，",
  "        // 未知变量会使整次组装失败（ADR-027 §3）。",
  "        interpolate: false,",
  "      });",
  "    } catch (error) {",
  "      console.error(`[aether-dsh-prompt] 注入失败：${error && error.message ? error.message : error}`);",
  "    }",
  "  });",
  "}",
  "",
].join("\n");

/** 把提示词插件落盘到 profile 的 `node_modules`（幂等覆盖；无 BOM）。 */
export function materializePromptPlugin(profileDir: string): MaterializedPlugin {
  const dir = join(profileDir, "node_modules", PROMPT_PLUGIN_PACKAGE_NAME);
  mkdirSync(dir, { recursive: true });
  const packageJsonPath = join(dir, "package.json");
  const indexPath = join(dir, "index.js");
  writeFileSync(packageJsonPath, PROMPT_PLUGIN_PACKAGE_JSON, "utf8");
  writeFileSync(indexPath, PROMPT_PLUGIN_INDEX_JS, "utf8");
  assertNoBom(packageJsonPath);
  assertNoBom(indexPath);
  return { dir, packageJsonPath, indexPath };
}

/**
 * 会话提示词侧信道文件路径（`<dir>/<encodeURIComponent(sessionId)>.md`）。
 *
 * DSH 的 `agent.id` 为 UUID；编码是防御纵深（避免 `native_id` 被写入带路径分隔符的
 * 文件名）。插件侧使用同一编码，见 `PROMPT_PLUGIN_INDEX_JS`。
 */
export function promptFilePath(promptDir: string, sessionId: string): string {
  return join(promptDir, `${encodeURIComponent(sessionId)}.md`);
}

/**
 * 清空侧信道目录中的 `.md` 文件（适配器启动时清理上次进程残留的提示词副本）。
 *
 * 只删除本插件命名空间内的文件；会话恢复时核心会重新下发并写入。删除失败
 * 尽力而为（不影响启动；残留最多是本地提示词副本，不进入日志/导出）。
 */
export function clearPromptDir(promptDir: string): number {
  let removed = 0;
  try {
    for (const entry of readdirSync(promptDir)) {
      if (!entry.endsWith(".md")) continue;
      try {
        unlinkSync(join(promptDir, entry));
        removed += 1;
      } catch {
        /* 单个文件删除失败不阻断启动 */
      }
    }
  } catch {
    /* 目录不可读不阻断启动 */
  }
  return removed;
}

export interface PatchOptions {
  provider?: string;
  model?: string;
  pluginName?: string;
  /** 提示词注入插件名（缺省 `aether-dsh-prompt`；测试可覆盖）。 */
  promptPluginName?: string;
  /**
   * `llm-pi-ai` 行的 composition base 配置（官方分层：插件行 config 为 base，
   * `settings.yaml` 为用户覆盖层）。声明 `{providers: {...}}` 使 provider 路由在
   * 插件 apply 时**同步注册**（与 `deepseek-official` 同路径），消除
   * 「session/new 早于 settings 异步注入」的冷启动竞态（M2-11 实测）。
   *
   * 值必须是 JSON（YAML 1.2 的合法子集）：以 JSON flow 形式内联 YAML。
   */
  providerConfig?: Record<string, unknown>;
  /** overlay 文件路径（缺省由 `writePatchFile` 生成到 DSH_HOME 下）。 */
  patchPath?: string;
}

/** 生成 overlay patch 文本（覆写 acp 行 + llm-pi-ai base 层 + insert 插件行）。 */
export function buildPatchText(options: PatchOptions): string {
  const lines: string[] = [];
  const provider = options.provider;
  const model = options.model;
  if (provider !== undefined || model !== undefined) {
    lines.push("- id: acp", "  config:");
    if (provider !== undefined) lines.push(`    provider: ${provider}`);
    if (model !== undefined) lines.push(`    model: ${model}`);
    lines.push("");
  }
  if (options.providerConfig !== undefined) {
    // JSON 是合法 YAML（flow 映射），直接内联避免手写 YAML 序列化器。
    lines.push("- id: llm-pi-ai", `  config: ${JSON.stringify(options.providerConfig)}`, "");
  }
  const streamPlugin = options.pluginName ?? PLUGIN_PACKAGE_NAME;
  const promptPlugin = options.promptPluginName ?? PROMPT_PLUGIN_PACKAGE_NAME;
  lines.push(
    "- insert:",
    `    - id: ${streamPlugin}`,
    `      name: ${streamPlugin}`,
    `    - id: ${promptPlugin}`,
    `      name: ${promptPlugin}`,
    "",
  );
  return lines.join("\n");
}

/** provider 配置形状校验（`{providers: {<name>: {...}}}`；CLI/核心传入均走此校验）。 */
export function validateProviderConfig(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("provider 配置必须是 JSON 对象");
  }
  const providers = (value as Record<string, unknown>)["providers"];
  if (typeof providers !== "object" || providers === null || Array.isArray(providers)) {
    throw new Error('provider 配置必须含 "providers" 对象');
  }
  if (Object.keys(providers).length === 0) {
    throw new Error("provider 配置的 providers 不得为空");
  }
  return value as Record<string, unknown>;
}

/** 写出 overlay 文件（`<DSH_HOME>/aether-dsh-<profile>.patch.yml`）；返回路径。 */
export function writePatchFile(home: string, profile: string, options: PatchOptions = {}): string {
  const patchPath = options.patchPath ?? join(home, `aether-dsh-${profile}.patch.yml`);
  mkdirSync(dirname(patchPath), { recursive: true });
  writeFileSync(patchPath, buildPatchText(options), "utf8");
  return patchPath;
}

/**
 * 从 `--dsh-bin`（如 `node_modules/@deepseek-ai/dsh/lib/bin.js`）向上解析包版本。
 *
 * 返回 `null` 表示无法解析（调用方按门闩失败处置）；解析到非 `@deepseek-ai/dsh`
 * 包名同样视为不可信。
 */
export function resolveDshVersion(binPath: string, maxDepth = 6): string | null {
  let current = resolve(binPath);
  for (let depth = 0; depth < maxDepth; depth += 1) {
    const candidate = join(current, "package.json");
    try {
      const parsed = JSON.parse(readFileSync(candidate, "utf8")) as {
        name?: string;
        version?: string;
      };
      if (parsed.name === "@deepseek-ai/dsh" && typeof parsed.version === "string") {
        return parsed.version;
      }
    } catch {
      /* 继续向上 */
    }
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return null;
}

/** 版本门闩校验结果。 */
export interface VersionLatchVerdict {
  ok: boolean;
  found: string | null;
  pin: string;
  detail: string;
}

/** 版本门闩判定（不匹配即拒绝加载 → `disabled + version_mismatch`）。 */
export function checkVersionLatch(found: string | null, pin: string): VersionLatchVerdict {
  if (found === pin) {
    return { ok: true, found, pin, detail: `DSH 版本 ${found} 与 pin 一致` };
  }
  const foundText = found ?? "未知";
  return {
    ok: false,
    found,
    pin,
    detail: `DSH 版本门闩失败：pin ${pin}，实际 ${foundText}。请安装 @deepseek-ai/dsh@${pin} 后重试（ADR-002 §3.1）`,
  };
}
