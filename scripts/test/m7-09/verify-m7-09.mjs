#!/usr/bin/env node
/**
 * M7-09 验证：Agent 预设（ADR-026 §2.2；DoD1–DoD6）。
 *
 * 覆盖：
 * - 静态登记：ADR-026 / ADR-027（DSH 系统提示词插件注入）/ 迁移 `0007` 与附录 C /
 *   命令面四命令 + 两错误码 / 审计动作 / 合成标记 / UI 锚点（`presets-page`、
 *   `preset-item`、`preset-edit`、`session-preset-select`）；
 * - DoD1：`aether-store m7_09_presets`（迁移结构/播种/外键/幂等 + 预设读写 + SET NULL）；
 * - DoD2：`aether-tauri m7_09_presets`（四命令全链路/内置保护/审计）+ `ipc_validation`
 *   预设校验矩阵；
 * - DoD3：`aether-control --lib memory::`（合成顺序与禁止静默覆盖单测）+
 *   `aether-tauri m7_09_presets`（会话创建合成 + 生效来源回显）；
 * - DoD4：`aether-tauri m7_09_presets mock_adapter_observes_composed_prompt_delivery`
 *   （Mock 适配器 session-log 观测 `session.create` 携带合成提示词；缺适配器**显式 SKIP**）；
 * - DoD5：前端 `m7_09_presets`（列表/内置不可删/编辑/新增/删除/使用中/新建入口透传）+
 *   typecheck；
 * - DoD6：M3-08 记忆注入回归（`pnpm verify:m3-08` 12 项）+ `m3_08_workspace` UI 回归 +
 *   `verify:m1-03`。
 *
 * 运行：node scripts/test/m7-09/verify-m7-09.mjs
 */
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");

const checks = [];
const skipped = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordSkip = (name, reason) => {
  skipped.push({ name, reason });
  console.log(`[m7-09] SKIP（显式）：${name} —— ${reason}`);
};

function vitest(packageName, files) {
  return run(pnpm, [...pnpmPrefix, "--filter", packageName, "exec", "vitest", "run", ...files]);
}

const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
const assertContains = (text, token, label) => {
  if (!text.includes(token)) throw new Error(`${label} 缺少：${token}`);
};

// ===== 静态登记与实现标记（任一缺失即失败） =====
try {
  // ADR-026 登记（M5-01 批次合入稿）。
  const adr = read("docs/adr/ADR-026-workspace-management-and-presets.md");
  for (const token of [
    "presets_list",
    "preset_add",
    "preset_update",
    "preset_delete",
    "preset_not_found",
    "builtin_preset_undeletable",
    "<!-- aether:preset:<preset_id> -->",
    "默认助手",
    "代码专家",
    "代码审查",
  ]) {
    assertContains(adr, token, "ADR-026");
  }

  // ADR-027：DSH 系统提示词插件注入（2026-10-20，设计 v1.17）。
  const adr027 = read("docs/adr/ADR-027-dsh-system-prompt-plugin-injection.md");
  for (const token of [
    "aether-dsh-prompt",
    "aether:session-prompt",
    "encodeURIComponent",
    "agent/created",
    "0.2.1-rc.2",
    "has_system_prompt",
    "system_prompt_effective",
    "per-profile/per-instance",
  ]) {
    assertContains(adr027, token, "ADR-027");
  }

  // ADR-006 附录 B：错误码登记行（M5-01 批次已登记，本任务实现）。
  const adr006 = read("docs/adr/ADR-006-startup-commands-and-error-codes.md");
  for (const code of ["preset_not_found", "builtin_preset_undeletable"]) {
    assertContains(adr006, `\`${code}\``, "ADR-006");
  }

  // UI-UX §5 文案行 + §7.3 锚点（M7-09 实现登记）。
  const uiux = read("docs/UI-UX-设计规格.md");
  for (const token of [
    "`preset_not_found`",
    "`builtin_preset_undeletable`",
    "`presets-page` / `preset-item` / `preset-edit`",
    "`session-preset-select`",
  ]) {
    assertContains(uiux, token, "UI-UX");
  }

  // 迁移 0007 + 框架内嵌 + 附录 C 终态 DDL 同批。
  const migration = read("migrations/0007_presets.sql");
  for (const token of [
    "CREATE TABLE presets",
    "is_builtin",
    "ALTER TABLE sessions ADD COLUMN preset_id TEXT REFERENCES presets(id) ON DELETE SET NULL",
    "'01J00000000000000000000P01'",
    "'01J00000000000000000000P02'",
    "'01J00000000000000000000P03'",
    "默认助手",
    "代码专家",
    "代码审查",
  ]) {
    assertContains(migration, token, "migrations/0007_presets.sql");
  }
  assertContains(read("crates/aether-store/src/migration.rs"), "0007_presets.sql", "migration.rs");
  const design = read("设计文档.md");
  for (const token of ["v1.17", "ADR-027", "aether-dsh-prompt", "0.2.1-rc.2"]) {
    assertContains(design, token, "设计文档");
  }
  const appendixStart = design.indexOf("## 附录 C");
  const codeStart = design.indexOf("```sql", appendixStart);
  const codeEnd = design.indexOf("```", codeStart + 6);
  const appendix = design
    .slice(codeStart + 6, codeEnd)
    .split(/\r?\n/)
    .map((line) => line.replace(/--.*$/, "").trim())
    .join("\n");
  for (const token of [
    "CREATE TABLE presets",
    "ALTER TABLE sessions ADD COLUMN preset_id",
  ]) {
    assertContains(appendix, token, "附录 C");
  }

  // 命令面：四命令函数 + 三处注册（collected / debug handler / release handler）。
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  for (const command of ["presets_list", "preset_add", "preset_update", "preset_delete"]) {
    assertContains(commands, `fn ${command}(`, "commands.rs");
    const occurrences = commands.split(`${command},`).length - 1;
    if (occurrences < 3) {
      throw new Error(`命令 ${command} 注册不足三处（collected + debug/release handler）`);
    }
  }
  assertContains(commands, "PresetAddRequest", "commands.rs");
  assertContains(commands, "PresetUpdateRequest", "commands.rs");
  assertContains(commands, "PresetDeleteRequest", "commands.rs");
  assertContains(commands, "PresetsListRequest", "commands.rs");

  // 错误码（ipc/error.rs）。
  const errors = read("crates/aether-app/src/ipc/error.rs");
  for (const token of [
    "PresetNotFound",
    '"preset_not_found"',
    "BuiltinPresetUndeletable",
    '"builtin_preset_undeletable"',
  ]) {
    assertContains(errors, token, "ipc/error.rs");
  }

  // DTO 校验上限（名称 ≤64 / 提示词 ≤32KiB；与核心常量同源）。
  const dto = read("crates/aether-app/src/ipc/dto.rs");
  for (const token of [
    "MAX_PRESET_NAME_CHARS",
    "MAX_PRESET_PROMPT_BYTES",
    "aether_core::PRESET_NAME_MAX_CHARS",
    "aether_core::PRESET_SYSTEM_PROMPT_MAX_BYTES",
    "pub preset_id: Option<String>",
  ]) {
    assertContains(dto, token, "ipc/dto.rs");
  }

  // 预设命令执行体：审计动作 + 内置保护 + 单写队列。
  const presetControl = read("crates/aether-app/src/preset_control.rs");
  for (const token of [
    "preset.created",
    "preset.updated",
    "preset.deleted",
    "builtin_preset_undeletable",
    "InsertPreset",
    "UpdatePreset",
    "DeletePreset",
  ]) {
    assertContains(presetControl, token, "preset_control.rs");
  }
  assertContains(read("crates/aether-tauri/src/lib.rs"), "with_presets", "lib.rs 组合根");

  // 合成口径（预设块 → 记忆块；禁止静默覆盖）。
  const memory = read("crates/aether-control/src/memory.rs");
  for (const token of ["compose_system_prompt", "PRESET_PROMPT_MARKER_PREFIX", "preset.prompt_block()"]) {
    assertContains(memory, token, "aether-control/src/memory.rs");
  }
  const backend = read("crates/aether-app/src/session_backend.rs");
  for (const token of [
    "compose_system_prompt",
    "with_preset(",
    "preset_id: preset.as_ref().map",
    "preset_name",
  ]) {
    assertContains(backend, token, "session_backend.rs");
  }

  // DoD4 适配器消费（Claude Code：`--append-system-prompt-file`；Codex/DSH 未实现，
  // 可行性结论见证据 §4.3）。
  const claudeCli = read("packages/adapter-claude-code/src/claude-cli.ts");
  for (const token of [
    "--append-system-prompt-file",
    "materializeSystemPromptFile",
    "cleanupSystemPromptFile",
  ]) {
    assertContains(claudeCli, token, "packages/adapter-claude-code/src/claude-cli.ts");
  }
  const claudeAdapter = read("packages/adapter-claude-code/src/claude-adapter.ts");
  for (const token of ["appendSystemPromptFile", "has_system_prompt", "system_prompt"]) {
    assertContains(claudeAdapter, token, "packages/adapter-claude-code/src/claude-adapter.ts");
  }
  const claudeAdapterTests = read("packages/adapter-claude-code/src/claude-adapter.test.ts");
  assertContains(
    claudeAdapterTests,
    "--append-system-prompt-file",
    "packages/adapter-claude-code/src/claude-adapter.test.ts",
  );

  // DoD4 适配器消费（Codex：`-c developer_instructions=…`；超长回退显式前缀块）。
  const codexCli = read("packages/adapter-codex/src/codex-cli.ts");
  for (const token of [
    "developer_instructions",
    "developerInstructionsLimit",
    "encodeDeveloperInstructions",
  ]) {
    assertContains(codexCli, token, "packages/adapter-codex/src/codex-cli.ts");
  }
  const codexAdapter = read("packages/adapter-codex/src/codex-adapter.ts");
  for (const token of [
    "systemPrompt",
    "SYSTEM_PROMPT_FALLBACK_HEADER",
    "developerInstructions",
    "has_system_prompt",
  ]) {
    assertContains(codexAdapter, token, "packages/adapter-codex/src/codex-adapter.ts");
  }
  const codexAdapterTests = read("packages/adapter-codex/src/codex-adapter.test.ts");
  assertContains(
    codexAdapterTests,
    "developer_instructions",
    "packages/adapter-codex/src/codex-adapter.test.ts",
  );

  // DoD4 适配器消费（DSH：`aether-dsh-prompt` 插件 + agent 作用域 section + 侧信道）。
  const dshPlugin = read("packages/adapter-dsh/src/dsh-plugin.ts");
  for (const token of [
    "PROMPT_PLUGIN_PACKAGE_NAME",
    "aether:session-prompt",
    "AETHER_DSH_PROMPT_DIR",
    "materializePromptPlugin",
    "encodeURIComponent",
    "agent.ctx.systemPrompt.section",
    "interpolate: false",
    "clearPromptDir",
  ]) {
    assertContains(dshPlugin, token, "packages/adapter-dsh/src/dsh-plugin.ts");
  }
  const dshAdapter = read("packages/adapter-dsh/src/dsh-adapter.ts");
  for (const token of [
    "materializePromptPlugin",
    "promptFilePath",
    "PROMPT_DIR_ENV",
    "has_system_prompt",
    "system_prompt_effective",
    "removePromptFile",
    "clearPromptDir",
  ]) {
    assertContains(dshAdapter, token, "packages/adapter-dsh/src/dsh-adapter.ts");
  }
  const dshPluginTests = read("packages/adapter-dsh/src/dsh-plugin.test.ts");
  for (const token of [
    "agent/created",
    "agent.ctx.systemPrompt.section",
    "promptFilePath",
    "interpolate",
    "clearPromptDir",
  ]) {
    assertContains(dshPluginTests, token, "packages/adapter-dsh/src/dsh-plugin.test.ts");
  }
  const dshAdapterTests = read("packages/adapter-dsh/src/dsh-adapter.test.ts");
  for (const token of ["system_prompt_effective", "toBe(true)", "toBe(false)", "toBeNull()"]) {
    assertContains(dshAdapterTests, token, "packages/adapter-dsh/src/dsh-adapter.test.ts");
  }

  // 前端：IPC 契约 + 锚点（presets-page/preset-item/preset-edit + 新建入口选择器）。
  const presetsTs = read("apps/desktop/src/presets.ts");
  for (const token of ["presets_list", "preset_add", "preset_update", "preset_delete"]) {
    assertContains(presetsTs, token, "apps/desktop/src/presets.ts");
  }
  const presetsPage = read("apps/desktop/src/PresetsPage.tsx");
  for (const token of [
    'data-testid="presets-page"',
    'data-testid="preset-item"',
    'data-testid="preset-edit"',
    'data-preset-id',
    'data-builtin',
    'data-active',
  ]) {
    assertContains(presetsPage, token, "PresetsPage.tsx");
  }
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of ['data-testid="session-preset-select"', "preset_id: selectedPresetId"]) {
    assertContains(workbench, token, "SessionWorkbench.tsx");
  }
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  for (const token of ['data-testid="settings-tab-presets"', "PresetsPage"]) {
    assertContains(settingsPage, token, "SettingsPage.tsx");
  }

  // 证据文档与验证入口存在（提交前归档）。
  if (!existsSync(path.join(repoRoot, "docs", "M7-09-证据.md"))) {
    throw new Error("缺少 docs/M7-09-证据.md（DoD 逐条证据归档）");
  }
  console.log("[static] ADR/迁移/附录 C/命令面/错误码/DTO/合成/UI/DSH 注入 登记一致");
  record("静态：ADR-026/027 + 迁移 0007 + 附录 C + 命令面/错误码/DTO/合成/UI/DSH 注入", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：ADR-026/027 + 迁移 0007 + 附录 C + 命令面/错误码/DTO/合成/UI/DSH 注入", 1);
}

// ===== Mock 适配器（DoD4/DoD6 共用；bun 不可用 → 显式 SKIP） =====
const bunBin =
  process.env.AETHER_BUN ?? path.join(process.env.USERPROFILE ?? "", ".bun", "bin", `bun${exeSuffix}`);
let mockAdapter = null;
if (!existsSync(bunBin)) {
  recordSkip("Mock 适配器构建（DoD4/DoD6）", `bun 不存在：${bunBin}`);
} else {
  const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-09", "bundle");
  mkdirSync(evidenceDir, { recursive: true });
  const outfile = path.join(evidenceDir, `aether-mock-adapter${exeSuffix}`);
  const built = run(
    bunBin,
    ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", outfile],
    { cwd: repoRoot },
  );
  record("Mock 适配器构建（bun --compile）", built);
  if (built === 0) mockAdapter = outfile;
}

const mockEnv = mockAdapter
  ? { AETHER_MOCK_ADAPTER: mockAdapter, AETHER_REQUIRE_MOCK_ADAPTER: "1" }
  : {};

// ===== DoD1：迁移/存储 =====
record(
  "DoD1 store m7_09_presets（迁移结构/播种/外键/幂等 + 预设读写 + SET NULL）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m7_09_presets"]),
);

// ===== DoD2/DoD3/DoD4：命令面 + 合成 + Mock 下发 =====
record(
  "DoD2/3/4 aether-tauri m7_09_presets（四命令/内置保护/审计/合成回显/Mock 下发）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_09_presets"], {
    env: mockAdapter ? mockEnv : {},
  }),
);
record(
  "DoD2 ipc_validation preset_commands_validation_matrix（校验矩阵）",
  run(cargo, [
    "test",
    "-p",
    "aether-tauri",
    "--test",
    "ipc_validation",
    "preset_commands_validation_matrix",
  ]),
);

// ===== DoD3：合成单测（aether-control） =====
record(
  "DoD3 aether-control --lib memory::（合成顺序/禁止覆盖/块标记）",
  run(cargo, ["test", "-p", "aether-control", "--lib", "memory::"]),
);

// ===== DoD4：真实运行时（opt-in；缺适配器/bun 显式 SKIP，禁止静默跳过） =====
if (process.env.AETHER_M7_09_REAL !== "1") {
  recordSkip(
    "DoD4 真实运行时（Claude Code 适配器线上 session.create 帧）",
    "未设置 AETHER_M7_09_REAL=1（opt-in；本地跑法见 docs/M7-09-证据.md §4）",
  );
} else if (!existsSync(bunBin)) {
  record("DoD4 真实运行时：构建 Claude 适配器 bundle（bun 缺失）", 1);
} else {
  const bundleDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-09", "bundle");
  mkdirSync(bundleDir, { recursive: true });
  const claudeBundle = path.join(bundleDir, `aether-claude-adapter${exeSuffix}`);
  const built = run(
    bunBin,
    [
      "build",
      "packages/adapter-claude-code/src/main.ts",
      "--compile",
      "--outfile",
      claudeBundle,
    ],
    { cwd: repoRoot },
  );
  if (built !== 0) {
    record("DoD4 真实运行时：Claude 适配器 bundle 构建失败（opt-in 场景不得跳过）", 1);
  } else {
    record(
      "DoD4 真实运行时：Claude Code 适配器收到合成提示词（线上 session.create；rpc-tap 观测）",
      run(
        cargo,
        [
          "test",
          "-p",
          "aether-tauri",
          "--test",
          "m7_09_presets",
          "real_claude_adapter_receives_composed_prompt_over_wire",
        ],
        {
          env: {
            AETHER_M7_09_REAL: "1",
            AETHER_CLAUDE_ADAPTER: claudeBundle,
          },
        },
      ),
    );
  }

  // Codex：真实适配器 + fake-codex，断言 `-c developer_instructions=…` 与解码一致。
  const codexBundle = path.join(bundleDir, `aether-codex-adapter${exeSuffix}`);
  const codexBuilt = run(
    bunBin,
    ["build", "packages/adapter-codex/src/main.ts", "--compile", "--outfile", codexBundle],
    { cwd: repoRoot },
  );
  if (codexBuilt !== 0) {
    record("DoD4 真实运行时：Codex 适配器 bundle 构建失败（opt-in 场景不得跳过）", 1);
  } else {
    record(
      "DoD4 真实运行时：Codex 适配器把合成提示词映射为 CLI developer_instructions（fake-codex 观测）",
      run(
        cargo,
        [
          "test",
          "-p",
          "aether-tauri",
          "--test",
          "m7_09_presets",
          "real_codex_adapter_passes_composed_prompt_to_cli",
        ],
        {
          env: {
            AETHER_M7_09_REAL: "1",
            AETHER_CODEX_ADAPTER: codexBundle,
          },
        },
      ),
    );
  }

  // DSH：真实 DSH 0.2.1-rc.2 + 适配器 + 插件 + 本地记录型上游（ADR-027）。
  // 优先使用 `AETHER_M7_09_DSH_ADAPTER` 预构建产物；否则用 bun 构建。
  let dshBundle = process.env.AETHER_M7_09_DSH_ADAPTER;
  if (dshBundle !== undefined && !existsSync(dshBundle)) {
    record("DoD4 真实 DSH：AETHER_M7_09_DSH_ADAPTER 指向的适配器不存在", 1);
    dshBundle = undefined;
  }
  if (dshBundle === undefined) {
    if (!existsSync(bunBin)) {
      recordSkip(
        "DoD4 真实 DSH：系统提示词插件注入 E2E",
        `bun 不存在且未提供 AETHER_M7_09_DSH_ADAPTER（${bunBin}）`,
      );
    } else {
      const dshBundlePath = path.join(bundleDir, `aether-dsh-adapter${exeSuffix}`);
      const dshBuilt = run(
        bunBin,
        ["build", "packages/adapter-dsh/src/main.ts", "--compile", "--outfile", dshBundlePath],
        { cwd: repoRoot },
      );
      if (dshBuilt !== 0) {
        record("DoD4 真实 DSH：适配器 bundle 构建失败（opt-in 场景不得跳过）", 1);
      } else {
        dshBundle = dshBundlePath;
      }
    }
  }
  if (dshBundle !== undefined) {
    const defaultDshBin =
      process.platform === "win32" && process.env.APPDATA
        ? path.join(
            process.env.APPDATA,
            "npm",
            "node_modules",
            "@deepseek-ai",
            "dsh",
            "lib",
            "bin.js",
          )
        : null;
    const dshBin = process.env.AETHER_M7_09_DSH_BIN ?? defaultDshBin;
    if (dshBin === null || !existsSync(dshBin)) {
      recordSkip(
        "DoD4 真实 DSH：系统提示词插件注入 E2E",
        "未找到 DSH（设置 AETHER_M7_09_DSH_BIN 或安装 @deepseek-ai/dsh@0.2.1-rc.2）",
      );
    } else {
      record(
        "DoD4 真实 DSH：插件注入 system 消息（真实 0.2.1-rc.2 + 本地记录型上游；ADR-027）",
        run(
          process.execPath,
          [
            path.join("scripts", "test", "m7-09", "dsh-prompt-e2e.mjs"),
            "--adapter",
            dshBundle,
            "--dsh-bin",
            dshBin,
          ],
          { cwd: repoRoot },
        ),
      );
    }
  }
}

// ===== DoD5：前端 =====
record(
  "DoD5 UI m7_09_presets（列表/内置不可删/编辑/新增/删除/使用中/新建入口 7 用例）",
  vitest("@aether/desktop", ["src/m7_09_presets.test.tsx"]),
);
record(
  "DoD5 前端 typecheck",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);
record(
  "DoD4 Claude 适配器消费单测（--append-system-prompt-file 映射 + 文件内容/清理）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/adapter-claude-code", "test"]),
);
record(
  "DoD4 Codex 适配器消费单测（-c developer_instructions 映射 + 超长回退前缀块）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/adapter-codex", "test"]),
);
record(
  "DoD4 DSH 适配器消费单测（插件包/agent 作用域/侧信道/清理；ADR-027）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/adapter-dsh", "test"]),
);

// ===== DoD6：M3-08 记忆注入回归（12 项）+ UI 回归 + 迁移 ↔ 附录 C =====
record(
  "DoD6 回归 pnpm verify:m3-08（记忆注入 12 项）",
  run(pnpm, [...pnpmPrefix, "verify:m3-08"]),
);
record(
  "DoD6 回归 UI m3_08_workspace（工作区绑定 4 用例）",
  vitest("@aether/desktop", ["src/m3_08_workspace.test.tsx"]),
);
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对；0007 同批合入）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== T14：bindings 生成物静态核验（完整 `git diff --exit-code` 门禁由提交后 CI 执行；
// 先例 M7-01/M7-02/M7-03/M7-06：本任务以「生成物含新契约 token」静态断言收口） =====
try {
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const token of [
    "presetsList",
    "presetAdd",
    "presetUpdate",
    "presetDelete",
    '"preset_not_found"',
    '"builtin_preset_undeletable"',
    "PresetAddRequest",
    "PresetUpdateRequest",
    "PresetDeleteRequest",
  ]) {
    assertContains(bindings, token, "packages/protocol/src/bindings.ts");
  }
  console.log("[t14] bindings 生成物含预设命令/错误码/请求类型（T14 由提交后 CI 关闭）");
  record("T14 bindings 生成物静态核验（预设命令/错误码/请求类型）", 0);
} catch (error) {
  console.error(`[t14] 失败：${error.message}`);
  record("T14 bindings 生成物静态核验（预设命令/错误码/请求类型）", 1);
}

if (skipped.length > 0) {
  console.log(`[m7-09] 显式 SKIP ${skipped.length} 项（见 summary）`);
}
process.exit(summarize("verify-m7-09", checks));
