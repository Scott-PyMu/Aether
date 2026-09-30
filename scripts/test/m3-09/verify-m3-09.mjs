/**
 * M3-09 验证入口：文件引用面板（只读；D3/D7/D9；ADR-010 决策 1/4）。
 *
 * 覆盖 DoD：
 *   1) 迁移 0003 `artifacts` 表/约束断言（`UNIQUE(session_id, path)`、
 *      `ON DELETE CASCADE`）；写路径经单写队列（D3）
 *      —— `aether-store --test m3_09_artifacts` + `aether-tauri --test m3_09_artifacts`
 *      + `verify:m1-03`（迁移集有效 schema ↔ 设计文档附录 C 逐项一致）；
 *   2) `artifact_add`：canonicalize + 可访问性检查（失败 `artifact_path_rejected`；
 *      合法路径不误拒）；文件/目录 `kind` 探测；重复添加幂等
 *      —— `m3_09_artifacts`（dod2_*）+ `ipc_validation`（路径校验器/校验矩阵）；
 *   3) `artifact_remove` 幂等（不存在 `removed=false`）；`artifacts_list` 排序与形状；
 *      跨重启保留 —— `m3_09_artifacts`（dod3_*）+ store 层用例；
 *   4) `ref_pick`：`{ kind }` 严格解析（未知 kind → `invalid_enum`、未知成员拒绝）；
 *      取消返回 `{ path: null }`；E2E 注入替身
 *      —— `aether-tauri --test m3_09_ref_pick`（MockRuntime + 选择器替身）；
 *   5) E2E：添加文件 → 会话文件；附加文件夹 → 项目文件；切换会话隔离；空态文案；
 *      `改动` tab 无入口 —— `apps/desktop`（m3_09_file_panel.test.tsx）；
 *   6) 边界断言：无目录列举/文件读取；不触发 `workspace_set`；不产生事件
 *      —— `m3_09_artifacts`（dod6_*）+ 本脚本静态守门；
 *   7) 折叠/断点行为按 UI-UX 规格（≥1280 展开、<1280 抽屉）—— 右栏容器 CSS
 *      （`right-panel[data-open]` + `@media (max-width: 1279px)`）+ 前端集成用例；
 *   8) T14 生成物更新（`ref_pick`/`artifacts_list`/`artifact_add`/`artifact_remove`
 *      + `SessionSummary` DTO）—— 重新生成幂等（changed=false）+ 生成物内容断言；
 *      严格 `git diff --exit-code` 由提交后的 CI（scripts/ci/bindings.mjs check）执行。
 *
 * 环境：Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；证据归档到
 *       `scripts/test/.tmp/m3-09/evidence-<stamp>/`（Gate 3 逐条出示）。
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
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-09", `evidence-${stamp}`);

// ===== 1. Rust 存储层：迁移 0003 表/约束 + 单写队列 + 幂等/排序/跨重启/无副作用 =====

record(
  "aether-store --test m3_09_artifacts（迁移 0003 约束/级联；单写队列幂等；删除幂等；跨重启；无事件/workspace 副作用）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_09_artifacts"], { cwd: repoRoot }),
);

// ===== 2. Rust 后端：artifacts_* IPC 后端（DoD2/3/6）+ ref_pick 命令层（DoD4）=====

mkdirSync(evidenceDir, { recursive: true });
const evidenceEnv = { AETHER_M3_09_EVIDENCE_DIR: evidenceDir };
record(
  "m3_09_artifacts（canonicalize/kind/幂等/路径拒绝矩阵/排序/跨重启/边界断言；证据归档）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_09_artifacts", "--", "--nocapture"], {
    cwd: repoRoot,
    env: evidenceEnv,
  }),
);
record(
  "m3_09_ref_pick（{kind} 严格解析/取消 null/选择器错误/未接线/后端不被触碰；证据归档）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_09_ref_pick", "--", "--nocapture"], {
    cwd: repoRoot,
    env: evidenceEnv,
  }),
);
record(
  "ipc_validation（校验矩阵扩展：4 命令畸形样本不落库 + 合法样本恰好一次到后端）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], { cwd: repoRoot }),
);

// ===== 3. 迁移集 ↔ 设计文档附录 C（含 0003 全量 DDL；ADR-010 冻结文本）=====
//
// 设计文档为本地基线（`.gitignore` 排除，不入库）：CI 上无该文件，此时跳过
// 附录 C 静态/实时比对（迁移 0003 的表/约束断言已由 m3_09_artifacts 测试覆盖，
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
    reason: "设计文档.md 不在仓库（.gitignore）；迁移 0003 约束断言由 m3_09_artifacts 覆盖",
  });
}

// ===== 4. 前端：面板 E2E（DoD5/7 锚点）+ 既有回归 =====

record(
  "pnpm --filter @aether/desktop test（文件面板：添加/附加/隔离/空态/改动无入口/错误码/删除 + 既有回归）",
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

// ===== 6. 静态守门：契约/接线/锚点/边界在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  const errorRs = read("crates/aether-tauri/src/ipc/error.rs");
  const pathRs = read("crates/aether-tauri/src/ipc/path.rs");
  const backend = read("crates/aether-tauri/src/session_backend.rs");
  const backendTrait = read("crates/aether-tauri/src/ipc/backend.rs");
  const bindings = read("crates/aether-tauri/src/bindings.rs");
  const migration = read("migrations/0003_p0_ui_extensions.sql");
  const embedded = read("crates/aether-store/src/migration.rs");
  const generated = read("packages/protocol/src/bindings.ts");
  const filePanel = read("apps/desktop/src/FilePanel.tsx");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const artifactsTs = read("apps/desktop/src/artifacts.ts");
  const styles = read("apps/desktop/src/styles.css");

  const required = [
    // 命令面（collected + 两个 handler 均有注册）。
    [commands, "ref_pick", "命令注册（ref_pick）"],
    [commands, "artifacts_list", "命令注册（artifacts_list）"],
    [commands, "artifact_add", "命令注册（artifact_add）"],
    [commands, "artifact_remove", "命令注册（artifact_remove）"],
    [backendTrait, "fn artifacts_list", "后端 trait（artifacts_list）"],
    [backendTrait, "fn artifact_add", "后端 trait（artifact_add）"],
    [backendTrait, "fn artifact_remove", "后端 trait（artifact_remove）"],
    // 错误码。
    [errorRs, "ArtifactPathRejected", "错误码枚举（ArtifactPathRejected）"],
    [errorRs, '"artifact_path_rejected"', "错误码稳定字符串"],
    // 路径校验（canonicalize + stat；不复用 A4 同步盘检测）。
    [pathRs, "pub fn validate_artifact_path", "引用路径校验入口"],
    [errorRs, "pub fn artifact_path_rejected", "统一拒绝码（artifact_path_rejected）"],
    // 后端接线与 DTO。
    [backend, "fn artifacts_list", "artifacts_list 真实后端"],
    [backend, "fn artifact_add", "artifact_add 真实后端"],
    [backend, "fn artifact_remove", "artifact_remove 真实后端"],
    [backend, "StoreCommand::InsertArtifact", "写路径经单写命令"],
    [backend, "StoreCommand::RemoveArtifact", "删除经单写命令"],
    [backend, "pub struct SessionSummary", "SessionSummary DTO 引入"],
    [backend, "workspace_root", "workspace_root 解析（sessions → workspaces）"],
    [bindings, ".typ::<SessionSummary>()", "T14：SessionSummary DTO 注册"],
    [bindings, ".typ::<RefPickRequest>()", "T14：ref_pick DTO 注册"],
    // 迁移内容（ADR-010 附录 A 冻结文本）。
    [migration, "CREATE TABLE artifacts", "迁移 0003 artifacts 表"],
    [migration, "UNIQUE (session_id, path)", "UNIQUE(session_id, path)"],
    [migration, "ON DELETE CASCADE", "会话级联删除"],
    [embedded, "0003_p0_ui_extensions.sql", "迁移 0003 内嵌"],
    // T14 生成物（禁止手改，此处仅断言内容存在）。
    [generated, "refPick", "生成物 ref_pick"],
    [generated, "artifactsList", "生成物 artifacts_list"],
    [generated, "artifactAdd", "生成物 artifact_add"],
    [generated, "artifactRemove", "生成物 artifact_remove"],
    [generated, "SessionSummary", "生成物 SessionSummary"],
    // 前端契约。
    [artifactsTs, '"ref_pick"', "前端 ref_pick 命令名"],
    [artifactsTs, '"artifacts_list"', "前端 artifacts_list 命令名"],
    [artifactsTs, '"artifact_add"', "前端 artifact_add 命令名"],
    [artifactsTs, '"artifact_remove"', "前端 artifact_remove 命令名"],
    [workbench, "<FilePanel", "工作台集成文件面板"],
    // 断点/折叠（≥1280 展开、<1280 抽屉）。
    [styles, "@media (max-width: 1279px)", "右栏 <1280 抽屉断点"],
    [styles, '.workbench-right[data-open="true"]', "右栏抽屉开关契约"],
    [styles, ".file-panel", "文件面板样式"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }

  // UI-UX §7.3 M3-09 锚点必须齐备（生产代码内出现）。
  const anchors = [
    "file-panel",
    "file-panel-empty",
    "file-panel-session-tab",
    "file-panel-project-tab",
    "ref-item",
    "ref-remove",
    "ref-add-file",
    "ref-add-folder",
    "ref-pick-error",
  ];
  for (const anchor of anchors) {
    if (!filePanel.includes(`data-testid="${anchor}"`)) {
      problems.push(`UI-UX 锚点缺失：${anchor}`);
    }
  }

  // 边界守门（DoD6）：引用路径校验不得复用 A4 同步盘检测；引用后端不得列目录/读文件。
  const validatorBody = /pub fn validate_artifact_path[\s\S]*?\n}/.exec(pathRs)?.[0] ?? "";
  if (validatorBody.includes("reject_cloud_sync_path")) {
    problems.push("validate_artifact_path 不得复用 A4 同步盘检测（ADR-010 评审裁定）");
  }
  const artifactsBackend = /fn artifacts_list[\s\S]*?\n    }\n/.exec(backend)?.[0] ?? "";
  const addBackend = /fn artifact_add[\s\S]*?\n    }\n/.exec(backend)?.[0] ?? "";
  for (const [source, label] of [
    [artifactsBackend, "artifacts_list"],
    [addBackend, "artifact_add"],
  ]) {
    for (const forbidden of ["read_dir", "std::fs::read(", "fs::read_to_string"]) {
      if (source.includes(forbidden)) {
        problems.push(`${label} 不得列目录/读文件内容（发现 ${forbidden}）`);
      }
    }
  }
  // 不触发 workspace_set：引用后端不引用 workspace_set 命令链接线。
  if (addBackend.includes("workspace_set") || artifactsBackend.includes("workspace_set")) {
    problems.push("引用后端不得触发 workspace_set（ADR-010 决策 1）");
  }
  // 前端不得出现「改动」tab 入口（P0 无入口）：仅检查 JSX 文本/字符串字面量，
  // 注释中的口径说明不构成入口。
  if (filePanel.includes(">改动<") || filePanel.includes('"改动"') || filePanel.includes("'改动'")) {
    problems.push("文件面板不得出现「改动」tab 入口（ADR-010 决策 1）");
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record(
    "静态检查：命令/DTO/迁移/生成物/锚点/断点/边界守门在案",
    problems.length === 0 ? 0 : 1,
  );
}

// ===== 7. 证据归档检查（逐用例 JSON；供 Gate 3 逐条出示）=====

{
  const files = existsSync(evidenceDir)
    ? readdirSync(evidenceDir).filter((name) => name.endsWith(".json"))
    : [];
  const expectFiles = [
    "dod1_migration_0003.json",
    "dod1_write_queue.json",
    "dod2_add_idempotent.json",
    "dod2_path_rejected.json",
    "dod3_list_restart.json",
    "dod4_ref_pick.json",
    "dod4_ref_pick_strict.json",
    "dod6_no_side_effects.json",
  ];
  const missing = expectFiles.filter((name) => !files.includes(name));
  if (missing.length > 0) console.error(`证据文件缺失：${missing.join(", ")}`);
  console.log(`[m3-09] 证据目录：${evidenceDir}`);
  record(
    "证据归档：8 份逐用例 JSON（迁移/写队列/幂等/路径拒绝/重启/ref_pick×2/无副作用）",
    missing.length === 0 ? 0 : 1,
  );
}

process.exit(summarize("verify-m3-09", checks));
