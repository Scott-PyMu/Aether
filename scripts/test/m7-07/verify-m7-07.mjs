#!/usr/bin/env node
/**
 * M7-07 验证入口：会话检索与管理（ADR-025 / P1-A11；直接实施项，不构成 P1 门禁）。
 *
 * 覆盖：
 * - DoD1：迁移 0008（FTS5 虚拟表 + 同步触发器 + rebuild）与附录 C / 迁移清单一致；
 *   校验脚本 `verify-m1-03.mjs` 已扩展 VIRTUAL TABLE/TRIGGER 解析；
 * - DoD2：`session_search` 标题/正文命中、标题加权、短语转义、分页；
 *   ≥1e5 会话 / ≥1e6 消息 P95 ≤300ms（`#[ignore]` 基准显式执行）；
 * - DoD3：`session_rename` 单写队列 + `session.updated` + 审计；
 * - DoD4：`session_delete` 硬删级联四表、`events` 不级联、`session.closed(reason=deleted)`、
 *   在途/待审批 `session_busy`、查无 `session_not_found`、删除后补读 `session_not_found`；
 * - DoD5：UI 搜索框/结果/改名/删除确认与 busy 错误；
 * - DoD6：M3-12 分组回归 + 桌面全量/typecheck + T14 bindings + verify:m1-03。
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-07", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

// ===== 1. 静态检查 =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  const commands = read("crates/aether-tauri/src/ipc/commands.rs");
  for (const name of ["session_search", "session_rename", "session_delete"]) {
    assert(commands.includes(`${name},`), `commands.rs 缺 ${name} 注册`);
  }
  const collected = /collect_commands!\[([\s\S]*?)\]/.exec(commands)?.[1] ?? "";
  const handlers = [...commands.matchAll(/generate_handler!\[([\s\S]*?)\]/g)].map(
    (match) => match[1],
  );
  assert(handlers.length === 2, "generate_handler 必须恰有两处（debug/release）");
  for (const name of ["session_search", "session_rename", "session_delete"]) {
    assert(new RegExp(`\\b${name}\\b`).test(collected), `collect_commands 缺 ${name}`);
    for (const block of handlers) {
      assert(new RegExp(`\\b${name}\\b`).test(block), `generate_handler 缺 ${name}`);
    }
  }

  const backend = read("crates/aether-tauri/src/ipc/backend.rs");
  for (const name of ["session_search", "session_rename", "session_delete"]) {
    assert(backend.includes(`fn ${name}`), `IpcBackend 缺 ${name}`);
  }
  const dto = read("crates/aether-tauri/src/ipc/dto.rs");
  for (const name of [
    "SessionSearchRequest",
    "SessionRenameRequest",
    "SessionDeleteRequest",
  ]) {
    assert(dto.includes(`pub struct ${name}`), `DTO 缺 ${name}`);
  }
  const error = read("crates/aether-tauri/src/ipc/error.rs");
  assert(error.includes("SessionNotFound") && error.includes('"session_not_found"'), "错误码缺 session_not_found");

  const bindings = read("packages/protocol/src/bindings.ts");
  for (const name of ["session_search", "session_rename", "session_delete"]) {
    assert(bindings.includes(`__TAURI_INVOKE("${name}"`), `bindings 缺 ${name}`);
  }
  for (const name of [
    "SessionSearchRequest",
    "SessionRenameRequest",
    "SessionDeleteRequest",
  ]) {
    assert(bindings.includes(`export type ${name}`), `bindings 缺类型 ${name}`);
  }

  const migration = read("migrations/0008_session_search_fts.sql");
  for (const token of [
    "CREATE VIRTUAL TABLE sessions_fts USING fts5",
    "CREATE VIRTUAL TABLE messages_fts USING fts5",
    "INSERT INTO sessions_fts(sessions_fts) VALUES('rebuild')",
    "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
    "trg_sessions_fts_ai",
    "trg_sessions_fts_ad",
    "trg_sessions_fts_au",
    "trg_messages_fts_ai",
    "trg_messages_fts_ad",
    "trg_messages_fts_au",
  ]) {
    assert(migration.includes(token), `0008 缺「${token}」`);
  }
  const manifest = read("crates/aether-store/src/migration.rs");
  assert(manifest.includes("0008_session_search_fts.sql"), "迁移清单缺 0008");

  const ops = read("crates/aether-store/src/ops.rs");
  for (const token of [
    "RenameSession",
    "DeleteSession",
    "SessionDeleted",
    "SessionSearchHit",
    "search_sessions",
    "events_retained",
  ]) {
    assert(ops.includes(token), `store 缺 ${token}`);
  }
  const lifecycle = read("crates/aether-control/src/lifecycle.rs");
  for (const token of [
    "rename_session",
    "delete_session",
    "session.renamed",
    "session.deleted",
    "SessionUpdatedPayload",
    "SessionClosedPayload",
    "reason: Some(\"deleted\"",
  ]) {
    assert(lifecycle.includes(token), `control 缺「${token}」`);
  }
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  assert(sessionBackend.includes("session_not_found"), "messages_page 缺删除后 session_not_found 检查");

  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of [
    'data-testid="session-search"',
    'data-testid="session-search-result"',
    'data-testid="session-rename"',
    'data-testid="session-rename-input"',
    'data-testid="session-rename-save"',
    'data-testid="session-delete"',
    'data-testid="session-delete-confirm"',
    'data-testid="session-delete-cancel"',
    'data-testid="session-manage-error"',
  ]) {
    assert(workbench.includes(token), `SessionWorkbench 缺「${token}」`);
  }
  const uiux = read("docs/UI-UX-设计规格.md");
  assert(uiux.includes("`session-search`") && uiux.includes("`session-delete-confirm`"), "UI-UX 锚点缺登记");
  const plan = read("docs/P1-实施计划与验收标准.md");
  assert(plan.includes("#### M7-07 会话检索与管理"), "P1 计划缺 M7-07 任务节");
  assert(plan.includes("M7-07") && plan.includes("执行状态：✅ 已完成"), "P1 计划缺 M7-07 执行状态");
  const rootPackage = JSON.parse(read("package.json"));
  assert(
    rootPackage.scripts?.["verify:m7-07"] === "node scripts/test/m7-07/verify-m7-07.mjs",
    "package.json 缺 verify:m7-07 脚本",
  );
  assert(existsSync(path.join(repoRoot, "docs/M7-07-证据.md")), "docs/M7-07-证据.md 缺失");

  const verifyM404 = read("scripts/test/m4-04/verify-m4-04.mjs");
  assert(verifyM404.includes('"session_search"') && verifyM404.includes('"session_not_found"'), "verify-m4-04 未同步命令/错误码");
  const commandCount = (collected.match(/\n\s*[a-z_]+,/g) ?? []).length;
  assert(commandCount === 63, `命令面应为 63，实际 ${commandCount}`);
  const errorEnum = /pub enum IpcErrorCode \{([\s\S]*?)\n\}/.exec(error)?.[1] ?? "";
  const errorCount = (errorEnum.match(/^\s*[A-Z][A-Za-z]+,$/gm) ?? []).length;
  assert(errorCount === 32, `错误码全集应为 32，实际 ${errorCount}`);

  record(
    "静态：命令面/错误码/bindings/迁移 0008/触发器/存储/控制/UI 锚点/计划登记逐项",
    0,
  );
} catch (failure) {
  record(`静态检查失败：${failure && failure.message ? failure.message : failure}`, 1);
}

// ===== 2. DoD1–DoD4：存储层（FTS/改名/删除/级联边界）=====
record(
  "DoD1-4 cargo test -p aether-store --test m7_07_session_search（3 用例）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m7_07_session_search"], {
    cwd: repoRoot,
  }),
);

// ===== 3. DoD2 性能基准（≥1e5 会话 / ≥1e6 消息；P95 ≤300ms）=====
record(
  "DoD2 搜索性能基准（1e5 会话/1e6 消息，P95 ≤300ms；忽略态显式执行）",
  run(
    cargo,
    [
      "test",
      "-p",
      "aether-store",
      "--test",
      "m7_07_session_search",
      "--",
      "--ignored",
      "--nocapture",
    ],
    { cwd: repoRoot },
  ),
);

// ===== 4. DoD2–DoD5：tauri 集成（事件/审计/删除后补读/参数矩阵）=====
record(
  "DoD2-5 cargo test -p aether-tauri --test m7_07_session_manage（4 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_07_session_manage"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD5 cargo test -p aether-tauri --test ipc_validation（含三命令畸形/合法样本）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"], {
    cwd: repoRoot,
  }),
);

// ===== 5. DoD5 前端：搜索/改名/删除确认 =====
record(
  "DoD5 前端 m7_07_session_management（5 用例：搜索/改名/删除确认/busy/清除）",
  run(pnpm, [
    ...pnpmPrefix,
    "--filter",
    "@aether/desktop",
    "exec",
    "vitest",
    "run",
    "src/m7_07_session_management.test.tsx",
  ]),
);

// ===== 6. DoD6 回归：M3-12 分组 + 桌面全量/typecheck + T14 + m1-03 =====
record(
  "DoD6 M3-12 分组/等待态回归（真实 Mock）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_12_waiting_state"], {
    cwd: repoRoot,
  }),
);
record(
  "DoD6 @aether/desktop 全量（含 M3-12/M7-04/M7-08）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "test"]),
);
record(
  "DoD6 @aether/desktop typecheck（tsc --noEmit）",
  run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]),
);
record(
  "T14 bindings 重新生成 + git diff --exit-code（git 基线）",
  run(process.execPath, [path.join(repoRoot, "scripts/ci/bindings.mjs"), "check"]),
);
record(
  "回归 verify:m1-03（迁移 0001–0008 ↔ 附录 C；FTS 虚拟表/触发器解析）",
  run(process.execPath, [path.join(repoRoot, "scripts/test/m1-03/verify-m1-03.mjs")]),
);

const summary = {
  task: "M7-07",
  generated_at: new Date().toISOString(),
  evidence_dir: path.relative(repoRoot, evidenceDir),
  checks,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m7-07] 证据目录：${evidenceDir}`);

process.exit(summarize("verify-m7-07", checks));
