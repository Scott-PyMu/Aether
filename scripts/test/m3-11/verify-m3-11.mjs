/**
 * M3-11 验证入口：模型与供应商配置（P0 UI + 数据模型；ADR-010 决策 3/4）。
 *
 * 覆盖 DoD：
 *   1) 迁移 0003 `providers`/`provider_models` 表/约束/播种（4 条内置、UNIQUE、
 *      ON DELETE CASCADE）；写路径经单写队列（D3）
 *      —— `aether-store --test m3_11_providers` + `aether-tauri --test m3_11_providers`
 *      + `verify:m1-03`（迁移集有效 schema ↔ 设计文档附录 C 逐项一致）；
 *   2) 命令矩阵：未知成员 / 非法 type / base_url 格式 / custom 缺 base_url /
 *      `api_key` 非空 >8192（too_large）/ 重复 model_id → 结构化错误且不落库
 *      —— `ipc_validation`（矩阵：畸形样本不达后端）+ `m3_11_providers`（后端重复拒绝）；
 *   3) 内置约束：`provider_delete` 内置 → `builtin_provider_undeletable`；
 *      `provider_toggle` 内置可停用；UI 删除入口置灰 —— `m3_11_providers` +
 *      `m3_11_providers.test.tsx`（DoD3/4）；
 *   4) 引用与密钥：`api_key` → aether-security 写密钥 → `api_key_ref`
 *      （`keychain://aether/provider/<id>`）；三态（缺省/空串/覆盖）；`providers_list`
 *      含引用不含明文；keychain 删除限自身命名空间（共享引用不删；不可用不阻断）；
 *      IPC 响应/诊断包明文 0 命中（含 `sk-`/`eyJ`/PEM 扫描）
 *      —— `m3_11_providers`（DoD4 四用例）+ 本脚本静态守门；
 *   5) 「测试连接」toast（无 IPC 调用、无网络请求）—— `m3_11_providers.test.tsx`（DoD5）；
 *   6) 模型选择器 E2E（仅启用供应商的启用模型；停用后移除；失效回退；空态文案；
 *      前置登记已落地）—— `m3_11_providers.test.tsx`（DoD6）+ 本脚本注册守门；
 *   7) 会话透传：选择模型 → `session.create.model` 断言；运行期只读（无 update 入口）
 *      —— `m3_11_providers.test.tsx`（DoD7）；
 *   8) T14 生成物更新 —— 重新生成幂等（changed=false）+ 生成物内容断言；
 *      严格 `git diff --exit-code` 由提交后的 CI（scripts/ci/bindings.mjs check）执行。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到
 *       `scripts/test/.tmp/m3-11/evidence-<stamp>/`（Gate 3 逐条出示）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-11", `evidence-${stamp}`);

// ===== 1. Rust 存储层：迁移 0003 表/约束/播种 + 单写队列 + 幂等/级联/跨重启 =====

record(
  "aether-store --test m3_11_providers（迁移 0003 表/约束/播种；单写队列；重复模型拒绝；级联删除；跨重启）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_11_providers"], { cwd: repoRoot }),
);

// ===== 2. Rust 后端：供应商七命令（DoD1–4；含密钥/诊断扫描）+ 校验矩阵 =====

mkdirSync(evidenceDir, { recursive: true });
const evidenceEnv = { AETHER_M3_11_EVIDENCE_DIR: evidenceDir };
record(
  "m3_11_providers（播种/清单/写队列；命令矩阵；内置约束；密钥三态/命名空间/诊断扫描；证据归档）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_11_providers", "--", "--nocapture"], {
    cwd: repoRoot,
    env: evidenceEnv,
  }),
);
record(
  "ipc_validation（校验矩阵扩展：七命令畸形样本不达后端 + 合法样本恰好一次到后端）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);

// ===== 3. 迁移集 ↔ 设计文档附录 C（含 0003 全量 DDL；ADR-010 冻结文本）=====
//
// 设计文档为本地基线（`.gitignore` 排除，不入库）：CI 上无该文件，此时跳过
// 附录 C 静态/实时比对（迁移 0003 的表/约束断言已由 m3_11_providers 测试覆盖，
// 二者均不依赖设计文档）；本地保留完整比对（DoD1 双保险）。

const designDoc = path.join(repoRoot, "设计文档.md");
if (existsSync(designDoc)) {
  record(
    "verify:m1-03（迁移集有效 schema ↔ 附录 C 逐项一致；0001/0002 未改）",
    run(node, [path.join(repoRoot, "scripts", "test", "m1-03", "verify-m1-03.mjs")]),
  );
} else {
  checks.push({
    name: "verify:m1-03（迁移集 ↔ 附录 C；设计文档未入库，本地专属）",
    skip: true,
    reason: "设计文档.md 不在仓库（.gitignore）；迁移 0003 约束断言由 m3_11_providers 覆盖",
  });
}

// ===== 4. 前端：供应商页/模型选择器 E2E（DoD3–7 锚点）+ 既有回归 =====

record(
  "pnpm --filter @aether/desktop test（供应商页/模型选择器/共存口径/透传 + 既有回归）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 5. T14：生成物幂等 + 内容断言 =====

{
  const result = spawnSync(node, [path.join(repoRoot, "scripts", "ci", "bindings.mjs"), "generate"], {
    cwd: repoRoot,
    encoding: "utf8",
    env: process.env,
  });
  const changed = /changed=false/.test(result.stdout ?? "");
  if (result.status !== 0 || !changed) {
    console.error(result.stdout ?? "");
    console.error(result.stderr ?? "");
  }
  record(
    "T14 生成物幂等：重新生成 changed=false（严格 git diff --exit-code 由提交后的 CI 执行）",
    result.status === 0 && changed ? 0 : 1,
  );
}

// ===== 6. 静态守门：契约/密钥/注册/锚点/边界在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const errorRs = read("crates/aether-tauri/src/ipc/error.rs");
  const backendTrait = read("crates/aether-tauri/src/ipc/backend.rs");
  const providerControl = read("crates/aether-tauri/src/provider_control.rs");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const bindingsRs = read("crates/aether-tauri/src/bindings.rs");
  const libRs = read("crates/aether-tauri/src/lib.rs");
  const migration = read("migrations/0003_p0_ui_extensions.sql");
  const generated = read("packages/protocol/src/bindings.ts");
  const providersTs = read("apps/desktop/src/providers.ts");
  const providersPage = read("apps/desktop/src/ProvidersPage.tsx");
  const modelSelector = read("apps/desktop/src/ModelSelector.tsx");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const settingsPage = read("apps/desktop/src/SettingsPage.tsx");
  const uiux = read("docs/UI-UX-设计规格.md");

  const commandNames = [
    "providers_list",
    "provider_create",
    "provider_update",
    "provider_delete",
    "provider_toggle",
    "provider_model_add",
    "provider_model_toggle",
  ];
  for (const name of commandNames) {
    if (!commands.includes(`pub(crate) fn ${name}`)) {
      problems.push(`命令层缺少：${name}`);
    }
    if (!commands.includes(`        ${name},`)) {
      problems.push(`命令注册（collected/handler）缺少：${name}`);
    }
    if (!backendTrait.includes(`fn ${name}`)) {
      problems.push(`IpcBackend trait 缺少：${name}`);
    }
    if (!generated.includes(name.replace(/_(.)/g, (_, c) => c.toUpperCase()))) {
      problems.push(`T14 生成物缺少：${name}`);
    }
    if (!providersTs.includes(`"${name}"`)) {
      problems.push(`前端命令名缺少：${name}`);
    }
  }

  const required = [
    // 错误码（ADR-010 附录 B.3）。
    [errorRs, "BuiltinProviderUndeletable", "错误码枚举（BuiltinProviderUndeletable）"],
    [errorRs, '"builtin_provider_undeletable"', "错误码稳定字符串（builtin_provider_undeletable）"],
    [errorRs, '"provider_not_found"', "错误码稳定字符串（provider_not_found）"],
    [errorRs, '"provider_model_not_found"', "错误码稳定字符串（provider_model_not_found）"],
    // 密钥写入路径（aether-security；D10）。
    [providerControl, "boot_secret_store", "密钥存储启动选择（keyring / A3 降级）"],
    [providerControl, "EncryptedFileStore::open_or_create", "A3 降级加密文件挂载"],
    [providerControl, "SecretValue::new", "密钥值包装（SecretValue）"],
    [providerControl, "provider_key_ref", "供应商密钥引用命名空间"],
    [providerControl, "keychain://aether/provider", "密钥引用 URI 口径注释"],
    [providerControl, "StoreCommand::InsertProvider", "供应商写路径经单写命令"],
    [providerControl, "StoreCommand::UpdateProvider", "供应商更新经单写命令"],
    [providerControl, "StoreCommand::DeleteProvider", "供应商删除经单写命令"],
    [providerControl, "StoreCommand::InsertProviderModel", "模型新增经单写命令"],
    [sessionBackend, "fn providers_list", "SessionBackend 转发（providers_list）"],
    [sessionBackend, "fn provider_create", "SessionBackend 转发（provider_create）"],
    [sessionBackend, "fn provider_model_toggle", "SessionBackend 转发（provider_model_toggle）"],
    [libRs, "provider_control::ProviderControl::new", "组合根接线（ProviderControl）"],
    [libRs, "boot_secret_store", "组合根接线（密钥存储）"],
    [bindingsRs, ".typ::<ProviderType>()", "T14：ProviderType 注册"],
    [bindingsRs, ".typ::<ProvidersListRequest>()", "T14：providers_list DTO 注册"],
    // 迁移内容（ADR-010 附录 A 冻结文本）。
    [migration, "CREATE TABLE providers", "迁移 0003 providers 表"],
    [migration, "CREATE TABLE provider_models", "迁移 0003 provider_models 表"],
    [migration, "UNIQUE (provider_id, model_id)", "UNIQUE(provider_id, model_id)"],
    [migration, "ON DELETE CASCADE", "外键级联删除"],
    [migration, "'01J00000000000000000000B01'", "内置播种 ULID（Anthropic）"],
    // 前端契约与 UI。
    [providersTs, "deriveModelGroups", "模型派生（仅启用供应商的启用模型）"],
    [providersTs, "ensureSelectedModelValid", "选中失效回退"],
    [providersTs, "OFFICIAL_PROVIDER_PRESETS", "官方预设（UI 侧常量，不落库）"],
    [settingsPage, "<ProvidersPage", "设置页集成供应商子页"],
    [workbench, "<ModelSelector", "工作台集成模型选择器"],
    [workbench, 'data-testid="session-model-input"', "UI-05 手动输入保留（共存口径）"],
    [modelSelector, "data-testid=\"model-selector-empty\"", "选择器空态锚点"],
    // 前置登记（M3-11 DoD6：ADR-010 §5-9 空态/手动输入共存口径登记先行）。
    [uiux, "模型选择器与手动输入共存（M3-11 前置登记", "UI-UX 前置登记（共存口径）"],
    [uiux, "不自动为新建会话预选模型", "UI-UX 前置登记（空选择不预选）"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // 明文守门（DoD4）：api_key 本体不得进入 provider_control 的响应/日志。
  if (/"api_key"\s*:/.test(providerControl)) {
    problems.push("provider_control 响应不得包含 api_key 字段（只允许 api_key_ref）");
  }
  const tracingWithKey = providerControl
    .split(/\r?\n/)
    .filter((line) => line.includes("tracing::") && line.includes("api_key"));
  if (tracingWithKey.length > 0) {
    problems.push("provider_control 不得把 api_key 写入 tracing 字段");
  }
  const providerJsonBody = /fn provider_json[\s\S]*?\n}/.exec(providerControl)?.[0] ?? "";
  if (!providerJsonBody.includes('"api_key_ref"') || providerJsonBody.includes('"api_key",')) {
    problems.push("provider_json 形状必须含 api_key_ref 且不含 api_key 本体");
  }
  if (!providerControl.includes("delete_own_key")) {
    problems.push("keychain 条目删除限自身命名空间（delete_own_key）缺失");
  }

  // UI-UX §7.3 M3-11 锚点必须齐备（生产代码内出现）。
  const anchors = [
    "providers-page",
    "provider-card",
    "provider-toggle",
    "provider-edit",
    "provider-delete",
    "provider-test",
    "provider-delete-confirm",
    "provider-delete-cancel",
    "provider-delete-confirm-button",
    "providers-add",
    "providers-add-custom",
    "provider-empty",
    "provider-error",
    "provider-test-notice",
    "providers-notice",
    "provider-type-select",
    "provider-form",
    "provider-name-input",
    "provider-base-url-input",
    "provider-api-key-input",
    "provider-api-key-reveal",
    "provider-api-key-ref-readonly",
    "provider-enabled-switch",
    "provider-form-save",
    "provider-form-back",
    "provider-model-item",
    "provider-model-toggle",
    "provider-model-add",
    "provider-model-id-input",
    "provider-model-name-input",
    "model-selector",
    "model-selector-toggle",
    "model-selector-list",
    "model-selector-search",
    "model-selector-item",
    "model-selector-empty",
    "settings-tab-general",
    "settings-tab-providers",
  ];
  for (const anchor of anchors) {
    if (
      !providersPage.includes(`data-testid="${anchor}"`) &&
      !modelSelector.includes(`data-testid="${anchor}"`) &&
      !settingsPage.includes(`data-testid="${anchor}"`)
    ) {
      problems.push(`UI-UX 锚点缺失：${anchor}`);
    }
  }

  // 「从供应商获取」P0 不提供入口（ADR-010 §5-7）：检查 JSX 文本入口（注释不构成入口）。
  if (/>\s*从供应商获取/.test(providersPage)) {
    problems.push("P0 不得提供「从供应商获取」入口（ADR-010 §5-7）");
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：命令/DTO/错误码/密钥路径/迁移/生成物/锚点/前置登记/明文守门在案",
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 7. 证据归档检查（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_seed_list.json",
    "dod2_command_matrix.json",
    "dod3_builtin_constraints.json",
    "dod4_key_ref.json",
    "dod4_key_tristate.json",
    "dod4_no_secret_store.json",
    "dod4_diagnostics_scan.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  console.log(`[m3-11] 证据目录：${evidenceDir}`);
  record(
    "证据归档：7 份逐用例 JSON（播种/命令矩阵/内置约束/密钥×3/诊断扫描）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-11", checks));
