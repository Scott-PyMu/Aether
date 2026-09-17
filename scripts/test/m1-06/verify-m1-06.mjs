/**
 * M1-06 验证入口（数据目录检测 / 迁移流 / 单实例锁；A4、D1、评审#9）。
 *
 * 覆盖：
 *   DoD1 Windows 三类样本全识别：OneDrive 环境变量前缀祖先、父目录
 *        FILE_ATTRIBUTE_REPARSE_POINT（含真实 Junction）、注册表 UserFolder
 *        （含真实注册表沙箱键）——Win runner 由 Rust 集成测试执行；
 *   DoD2 macOS 两类样本全识别：iCloud 容器、File Provider（~/Library/CloudStorage）
 *        ——注入 home 上下文可在任意宿主断言（含降级精度提示文案）；
 *        macOS runner 由 `scripts/test/m1-06/verify-m1-06-macos.mjs` 走原生上下文
 *        （真实样本目录 + generate/clean 脚本 + 证据 artifact）；
 *   DoD3 命中后仅「迁移/退出」可达（主界面不可达）、迁移后锁定新目录 —— E2E；
 *   DoD4 本地目录对照样本 20 个全部放行（防误杀）—— Rust 集成测试；
 *   DoD5 单实例：二次启动聚焦已有窗口、第二进程退出、无双写连接 —— E2E；
 *   回归：aether-tauri 全量测试（含 M1-08 校验矩阵）保持通过。
 *
 * 用法：node scripts/test/m1-06/verify-m1-06.mjs [--skip-e2e] [--skip-e2e-build]
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const args = process.argv.slice(2);
const skipE2e = args.includes("--skip-e2e");
const skipE2eBuild = args.includes("--skip-e2e-build");
// 真实系统选择器冒烟需交互桌面（前台校验 + 键盘注入），默认不在 CI 运行。
const withPickerSmoke = args.includes("--with-picker-smoke");

const env = buildEnv();
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m1-06", stamp);
mkdirSync(outDir, { recursive: true });

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordOk = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

// ---------------------------------------------------------------------------
// 1. Rust 集成测试（全量 aether-tauri，含 M1-06 样本与 M1-08 回归）
// ---------------------------------------------------------------------------
const testResult = spawnSync(
  bin("cargo"),
  ["test", "-p", "aether-tauri", "--", "--nocapture"],
  { cwd: repoRoot, env, encoding: "utf8" },
);
const output = `${testResult.stdout ?? ""}\n${testResult.stderr ?? ""}`;
writeFileSync(path.join(outDir, "cargo-test-aether-tauri.txt"), output);
process.stdout.write(output);
record("Rust：aether-tauri 全量测试（M1-06 + M1-08 回归）", testResult.status ?? 1);

// ---------------------------------------------------------------------------
// 2. DoD 样本报告（从测试输出提取）
// ---------------------------------------------------------------------------
const requiredLines = [
  [
    "[m1-06] sample win.one_drive_env_prefix",
    "DoD1 ① Windows：OneDrive 环境变量前缀祖先命中",
  ],
  [
    "[m1-06] sample win.parent_reparse_point",
    "DoD1 ② Windows：父目录 FILE_ATTRIBUTE_REPARSE_POINT 命中（注入 + 真实 Junction）",
  ],
  [
    "[m1-06] sample win.reg_user_folder",
    "DoD1 ③ Windows：注册表 UserFolder 比对命中（注入 + 真实沙箱键）",
  ],
  [
    "[m1-06] sample mac.icloud_ubiquitous",
    "DoD2 ① macOS：iCloud 容器命中（路径前缀降级，UI 明示精度限制）",
  ],
  [
    "[m1-06] sample mac.file_provider_path",
    "DoD2 ② macOS：File Provider（~/Library/CloudStorage）命中",
  ],
];
for (const [needle, label] of requiredLines) {
  recordOk(label, output.includes(needle), output.includes(needle) ? "hit" : "未观测到样本命中行");
}
recordOk(
  "DoD2 降级：macOS 精度限制提示已输出（检测精度受限 + 手动确认指引）",
  output.includes("[m1-06] macos precision-note:") &&
    output.includes("检测精度受限") &&
    output.includes("请确认目录不在 iCloud/CloudStorage 下"),
);
recordOk(
  "DoD4 本地对照样本 20/20 放行（防误杀）",
  output.includes("[m1-06] controls summary: 20/20"),
);

const sampleLines = output
  .split(/\r?\n/)
  .filter((line) => line.startsWith("[m1-06]"));
writeFileSync(
  path.join(outDir, "sample-report.json"),
  `${JSON.stringify({ lines: sampleLines }, null, 2)}\n`,
);
console.log(`[m1-06] 样本报告已归档：${path.join(outDir, "sample-report.json")}`);

// ---------------------------------------------------------------------------
// 3. E2E：拒绝启动流 + 迁移 + 锁定 + 单实例（真实 WebView2）
// ---------------------------------------------------------------------------
let e2eOutput = "";
if (skipE2e) {
  recordOk("DoD3/DoD5 E2E（拒绝启动→迁移→锁定新目录 + 单实例聚焦）", true, "跳过（--skip-e2e）");
} else {
  const e2eArgs = [path.join(repoRoot, "scripts", "test", "m1-06", "e2e-startup-guard.mjs")];
  if (skipE2eBuild) e2eArgs.push("--skip-frontend-build", "--skip-rust-build");
  const e2e = spawnSync(process.execPath, e2eArgs, { cwd: repoRoot, env, encoding: "utf8" });
  e2eOutput = `${e2e.stdout ?? ""}\n${e2e.stderr ?? ""}`;
  writeFileSync(path.join(outDir, "e2e-startup-guard.txt"), e2eOutput);
  process.stdout.write(e2eOutput);
  record("DoD3/DoD5 E2E（拒绝启动→迁移→锁定新目录 + 单实例聚焦）", e2e.status ?? 1);
  recordOk(
    "DoD3 迁移主路径：注入 DirectoryPicker → picked 回报出现在 E2E 输出",
    e2eOutput.includes('"stage":"picked"'),
    e2eOutput.includes('"stage":"picked"') ? "picked" : "未观测到 picked 回报",
  );
}

// ---------------------------------------------------------------------------
// 4. 静态：拒绝启动界面不提供覆盖开关（评审 #9）
// ---------------------------------------------------------------------------
{
  const source = readFileSync(
    path.join(repoRoot, "apps", "desktop", "src", "StartupGate.tsx"),
    "utf8",
  );
  const banned = ["仍要在此目录运行", "继续运行", "override", "forceStart"];
  const hits = banned.filter((token) => source.includes(token));
  if (hits.length > 0) {
    console.error(`拒绝启动界面出现覆盖开关字样：${hits.join(",")}`);
  }
  record("评审#9：拒绝启动界面无「仍要在此目录运行」覆盖开关", hits.length > 0 ? 1 : 0);
}

// ---------------------------------------------------------------------------
// 5. 静态：macOS 降级说明常量 / UI 渲染 / mac 样本与 CI 接线
// ---------------------------------------------------------------------------
{
  const detectSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "startup", "detect.rs"),
    "utf8",
  );
  const noteOk =
    detectSource.includes("pub const MAC_PRECISION_NOTE") &&
    detectSource.includes("检测精度受限") &&
    detectSource.includes("请确认目录不在 iCloud/CloudStorage 下");
  recordOk("detect.rs：MAC_PRECISION_NOTE 常量含指定降级文案", noteOk);

  const gateSource = readFileSync(
    path.join(repoRoot, "apps", "desktop", "src", "StartupGate.tsx"),
    "utf8",
  );
  recordOk(
    "StartupGate.tsx：启动门渲染 startup-precision-note（UI 明示）",
    gateSource.includes("startup-precision-note") && gateSource.includes("detection.note"),
  );

  recordOk(
    "macOS 样本脚本存在（scripts/test/macos_sync_samples.sh）",
    existsSync(path.join(repoRoot, "scripts", "test", "macos_sync_samples.sh")),
  );
  const workflow = readFileSync(
    path.join(repoRoot, ".github", "workflows", "ci.yml"),
    "utf8",
  );
  recordOk(
    "CI：data-dir-guard-macos job 调用 macOS 验证器并上传证据 artifact",
    workflow.includes("verify-m1-06-macos.mjs") &&
      workflow.includes("m1-06-macos-evidence") &&
      workflow.includes("macos-14"),
  );
}

// ---------------------------------------------------------------------------
// 6. 静态：目录选择器抽象与迁移分支测试接线
// ---------------------------------------------------------------------------
{
  const pickerSource = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "src", "picker.rs"),
    "utf8",
  );
  recordOk(
    "picker.rs：DirectoryPicker trait + 生产/替身实现",
    pickerSource.includes("pub trait DirectoryPicker") &&
      pickerSource.includes("TauriDialogPicker") &&
      pickerSource.includes("FixedDirectoryPicker"),
  );

  const migrationTest = readFileSync(
    path.join(repoRoot, "crates", "aether-tauri", "tests", "m1_06_migration.rs"),
    "utf8",
  );
  const branches = [
    "migration_rejects_non_directory_target",
    "migration_rejects_unwritable_target",
    "migration_rejects_insufficient_space",
    "migration_rejects_invalid_and_sync_targets",
    "gate_rejects_sync_or_non_empty_migration_target",
  ];
  const missingBranches = branches.filter((name) => !migrationTest.includes(name));
  recordOk(
    "迁移校验分支测试：不存在/非目录/不可写/空间不足/同步盘/目标非空",
    missingBranches.length === 0,
    missingBranches.join(","),
  );

  recordOk(
    "E2E 注入选择器环境变量（AETHER_E2E_PICK_DIR）接线",
    readFileSync(
      path.join(repoRoot, "crates", "aether-tauri", "src", "startup_probe.rs"),
      "utf8",
    ).includes("AETHER_E2E_PICK_DIR"),
  );
}

// ---------------------------------------------------------------------------
// 7. 真实系统选择器冒烟（opt-in：需交互桌面）
// ---------------------------------------------------------------------------
if (withPickerSmoke) {
  const smokeArgs = [
    path.join(repoRoot, "scripts", "test", "m1-06", "manual-picker-smoke.mjs"),
  ];
  if (skipE2eBuild) smokeArgs.push("--skip-frontend-build", "--skip-rust-build");
  const smoke = spawnSync(process.execPath, smokeArgs, { cwd: repoRoot, env, encoding: "utf8" });
  const smokeOutput = `${smoke.stdout ?? ""}\n${smoke.stderr ?? ""}`;
  writeFileSync(path.join(outDir, "manual-picker-smoke.txt"), smokeOutput);
  process.stdout.write(smokeOutput);
  record("真实系统选择器冒烟（opt-in；截图与日志归档）", smoke.status ?? 1);
} else {
  recordOk(
    "真实系统选择器冒烟（opt-in；--with-picker-smoke）",
    true,
    "跳过（需交互桌面）",
  );
}

// 失败项单行输出（CI 日志 tail 可捕获；含 E2E 的具体错误）。
for (const check of checks) {
  const failed = !check.skip && (check.expect === "nonzero" ? check.exit === 0 : check.exit !== check.expect);
  if (failed) console.log(`[m1-06] failed-check ${check.name}`);
}
// 不用 process.exit：避免管道输出未 flush 导致 CI 日志截断（诊断需要完整 FAIL 行）。
process.exitCode = summarize("verify-m1-06", checks);
