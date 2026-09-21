/**
 * M2-09 验证入口：大行与 OOM 防护（设计 D6/D2、评审#6；依赖 M1-09、M2-01）。
 *
 * 覆盖 DoD：
 *   1) >2MiB 任意行：不缓冲、断连、记错；1–2MiB 非引用行正常解析（内存曲线断言受控，
 *      真实 Mock 进程 + 核心侧 RSS 采样）；
 *   2) `artifact_ref` 大附件引用流正常：附件不落库、仅存 artifacts 路径
 *      （引用帧只携带路径 + 元数据；标记字节在任何已解析帧 0 命中；aether-adapters
 *      无存储依赖 ⇒ 数据体无落库路径；路径逃逸/形状非法被拒）；
 *   3) 1.5GiB 压力注入 → 告警 + 限流且不崩溃（与 M2-07 联动：同一 ResourcePatrol +
 *      `AETHER_TEST_RSS_*` env 阈值钩子；真实分配 → Alert/Throttled 事件落盘 →
 *      delta 限流窗口 → 释放回落 → 管线续写正常）。
 *
 * 用法：node scripts/test/m2-09/verify-m2-09.mjs
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；Bun 编译 Mock（AETHER_BUN 或
 *       ~/.bun/bin/bun[.exe]）；m2_09_oom 需 ~1.5GiB 内存（Windows CI 7GB 满足）。
 */
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const bun = resolveBun();
const mockBinary = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m1-09",
  process.platform === "win32" ? "aether-mock-adapter.exe" : "aether-mock-adapter",
);

// ===== 0. 编译 Mock 单文件（真实进程端到端前提） =====

record(
  "编译 Mock 适配器单文件（bun build --compile）",
  run(
    bun,
    ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockBinary],
    { cwd: repoRoot },
  ),
);

const mockEnv = {
  AETHER_MOCK_ADAPTER: mockBinary,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
};

// ===== 1. DoD1/DoD2：Rust 单测 + 真实 Mock 进程集成 =====

record(
  "cargo test -p aether-adapters --lib（artifact 校验器/连接分发/监督器附件目录/帧层回归）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]),
);
record(
  "m2_09_artifacts（DoD2：3MiB 附件仅存 artifacts 路径 + 线协议零数据体 + 逃逸/形状拒绝 + 监督器注入）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_09_artifacts"], {
    env: mockEnv,
  }),
);
record(
  "m2_09_memory（DoD1：>2MiB 连发断连记错 + 1–2MiB 连发正常解析；核心 RSS 增长受控）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m2_09_memory"], {
    env: mockEnv,
  }),
);

// ===== 2. DoD3：1.5GiB 真实压力（与 M2-07 联动） =====

record(
  "m2_09_oom（DoD3：1.5GiB 压力 → core_rss_alert/core_rss_throttle 落盘 → delta 限流 → 释放回落 → 不崩溃；--nocapture）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_09_oom",
    "--",
    "--nocapture",
  ]),
);

// ===== 3. TS 侧：SDK 全帧形状 + Mock 附件触发 =====

record(
  "adapter-sdk 单测（artifact_ref 全帧形状/notifyFrame/emitArtifactRef）",
  pnpmRun(["--filter", "@aether/adapter-sdk", "test"]),
);
record(
  "adapter-mock 单测（artifact: 触发/目录缺失失败/逃逸与形状非法注入/连发注入）",
  pnpmRun(["--filter", "@aether/adapter-mock", "test"]),
);

// ===== 4. 静态检查与边界验证 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");

  // DoD1：帧上限常量冻结（2MiB / 64KiB；ADR-004）。
  const framing = read("crates/aether-adapters/src/framing.rs");
  for (const [needle, label] of [
    ["pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024", "D6 单帧硬上限 2MiB"],
    ["pub const ARTIFACT_REF_LIMIT: usize = 1024 * 1024", "artifact_ref <1MiB 契约"],
    ["pub const READ_CHUNK_BYTES: usize = 64 * 1024", "读取块限流（缓冲上界）"],
  ]) {
    if (!framing.includes(needle)) problems.push(`framing.rs 缺少${label}: ${needle}`);
  }

  // DoD2：核心侧校验器 + 连接分发 + 监督器注入接线在案。
  const artifact = read("crates/aether-adapters/src/artifact.rs");
  for (const [needle, label] of [
    ["pub struct ArtifactValidator", "引用帧校验器"],
    ["pub enum ArtifactError", "校验错误类型"],
    ["Component::ParentDir", "`..` 段拒绝"],
    ["resolved.starts_with(&self.root)", "canonicalize 前缀比较"],
    ["SizeMismatch", "尺寸核对"],
  ]) {
    if (!artifact.includes(needle)) problems.push(`artifact.rs 缺少${label}: ${needle}`);
  }
  const connection = read("crates/aether-adapters/src/connection.rs");
  for (const [needle, label] of [
    ["ArtifactRef(Box<ArtifactRefParams>)", "通知变体"],
    ['notify::ARTIFACT_REF =>', "方法分发"],
    ["artifact_ref 引用帧校验失败", "形状非法按无效帧计数"],
  ]) {
    if (!connection.includes(needle)) problems.push(`connection.rs 缺少${label}: ${needle}`);
  }
  const supervisor = read("crates/aether-adapters/src/supervisor/runtime.rs");
  for (const [needle, label] of [
    ['pub const ENV_ARTIFACTS_DIR: &str = "AETHER_ARTIFACTS_DIR"', "环境注入常量"],
    ["with_artifacts_dir", "RuntimeSpec 附件目录"],
    ["create_dir_all", "附件目录创建"],
  ]) {
    if (!supervisor.includes(needle)) problems.push(`runtime.rs 缺少${label}: ${needle}`);
  }
  const mock = read("packages/adapter-mock/src/mock-adapter.ts");
  for (const [needle, label] of [
    ['ARTIFACT_TRIGGER_PREFIX = "artifact:"', "artifact 触发前缀"],
    ["emitArtifactRef", "引用帧上报"],
    ["ARTIFACT_MARKER", "附件内容标记"],
    ['"artifact-ref-outside"', "逃逸注入"],
    ['"line-over-2mib-burst"', ">2MiB 连发注入"],
  ]) {
    if (!mock.includes(needle)) problems.push(`mock-adapter.ts 缺少${label}: ${needle}`);
  }
  const sdk = read("packages/adapter-sdk/src/protocol.ts");
  for (const [needle, label] of [
    ['export const ARTIFACT_REF_METHOD = "artifact_ref"', "方法名常量"],
    ["export interface ArtifactRefParams", "全帧形状"],
    ["export interface ArtifactRefEntry", "引用条目形状"],
  ]) {
    if (!sdk.includes(needle)) problems.push(`protocol.ts 缺少${label}: ${needle}`);
  }

  // DoD3：与 M2-07 联动的 env 阈值钩子冻结。
  const patrol = read("crates/aether-control/src/resource_patrol.rs");
  for (const [needle, label] of [
    ['ENV_RSS_ALERT_MB: &str = "AETHER_TEST_RSS_ALERT_MB"', "告警阈值钩子"],
    ['ENV_RSS_THROTTLE_MB: &str = "AETHER_TEST_RSS_THROTTLE_MB"', "限流阈值钩子"],
  ]) {
    if (!patrol.includes(needle)) problems.push(`resource_patrol.rs 缺少${label}: ${needle}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-09 大行/引用流/OOM 关键接线在案", problems.length === 0 ? 0 : 1);
}

// 边界（非功能验证）：DoD2「附件不落库」——aether-adapters 链路无存储依赖，
// 数据体不进入线协议 ⇒ 附件内容无任何路径进入 events 表（附录 B 亦无附件事件类型）。
{
  const manifest = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "Cargo.toml"),
    "utf8",
  );
  const hits = ["aether-store", "rusqlite"].filter((name) =>
    new RegExp(`^\\s*${name}[\\s.]`, "m").test(manifest),
  );
  console.log(
    `[m2-09 边界] aether-adapters 依赖中的存储项 = ${JSON.stringify(hits)}（应为空 → 附件数据体无落库路径）`,
  );
  record(
    "边界验证（非功能验证）：aether-adapters 无存储依赖（附件不落库的结构性前提）",
    hits.length === 0 ? 0 : 1,
  );
}

// ===== 5. clippy（新代码 lint 门禁） =====

record(
  "cargo clippy -p aether-adapters -p aether-control --all-targets -- -D warnings（unwrap/expect/panic 为 deny 级）",
  run(cargo, [
    "clippy",
    "-p",
    "aether-adapters",
    "-p",
    "aether-control",
    "--all-targets",
    "--",
    "-D",
    "warnings",
  ]),
);

process.exit(summarize("verify-m2-09", checks));