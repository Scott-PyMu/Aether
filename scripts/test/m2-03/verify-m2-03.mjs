/**
 * M2-03 验证入口：权限网关与审批（设计 D9、评审 #1/#10）。
 *
 * 覆盖 DoD：
 *   1) 策略矩阵（fs.read 工作区 allow/外 deny；fs.write 工作区 ask/外 deny；exec deny；
 *      记忆文件白名单 allow，1MB 上限）——aether-security 单测 + aether-control 集成；
 *   2) 路径逃逸样本集 100% deny + 审计（T7：`..`/软链接/Junction/UNC/8.3/ADS/
 *      尾随点空格/保留设备名；文本样本全平台，文件系统样本按平台可创建性显式记录）；
 *   3) 审批流：pending 持久化、重启恢复、300s 超时 deny + 审计（时钟注入）；
 *   4) T6：100 次并发 ask 无丢失/重复/死锁；
 *   5) 边界口径（D9/AGENTS §2.7）：仅约束经线协议上报的工具调用——文档检查 +
 *      审计字段（request_id/target/runtime_id）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析。
 */
import path from "node:path";
import process from "node:process";
import { readFileSync } from "node:fs";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");

// ===== DoD1/DoD2（决策层）：策略矩阵 + 路径校验 + 审批票据单测 =====

record(
  "cargo test -p aether-security --lib（策略矩阵/记忆白名单/T7 文本样本/审批 300s 截止）",
  run(cargo, ["test", "-p", "aether-security", "--lib"]),
);

// ===== DoD1–DoD5（集成，真实存储 + 审计）：--nocapture 输出 T7 汇总行 =====

record(
  "m2_03_permission（矩阵/逃逸 100% deny+审计/重启恢复/300s 超时/T6 100 并发，原始输出见日志）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_03_permission",
    "--",
    "--nocapture",
  ]),
);

// ===== DoD5：边界口径文档检查（D9 评审修订 #1 / AGENTS §2.7） =====

{
  const module = readFileSync(
    path.join(repoRoot, "crates", "aether-security", "src", "permission", "mod.rs"),
    "utf8",
  );
  const policy = readFileSync(
    path.join(repoRoot, "crates", "aether-security", "src", "permission", "policy.rs"),
    "utf8",
  );
  const guard = readFileSync(
    path.join(repoRoot, "crates", "aether-security", "src", "permission", "path.rs"),
    "utf8",
  );
  const approval = readFileSync(
    path.join(repoRoot, "crates", "aether-security", "src", "permission", "approval.rs"),
    "utf8",
  );
  const service = readFileSync(
    path.join(repoRoot, "crates", "aether-control", "src", "permission.rs"),
    "utf8",
  );
  const problems = [];
  const boundary = "仅约束适配器经线协议上报的工具调用";
  if (!module.includes(boundary) || !policy.includes("适配器进程内行为")) {
    problems.push("边界口径声明缺失（security::permission 模块文档）");
  }
  if (!service.includes("适配器进程内行为不经此门")) {
    problems.push("边界口径声明缺失（control::permission 服务文档）");
  }
  const required = [
    ["APPROVAL_TIMEOUT_MS: i64 = 300_000", "300s 审批超时常量"],
    ["MEMORY_FILE_MAX_BYTES: u64 = 1_048_576", "记忆文件 1MB 上限"],
    ["T7_TEXTUAL_SAMPLES", "T7 文本样本集"],
    ["case_insensitive_platform", "平台大小写规则"],
    ["permission.timeout", "超时审计动作"],
    ["permission.denied_by_policy", "策略拒绝审计动作"],
  ];
  const combined = `${module}\n${policy}\n${guard}\n${approval}\n${service}`;
  for (const [needle, label] of required) {
    if (!combined.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-03 常量/边界声明/审计动作在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m2-03", checks));
