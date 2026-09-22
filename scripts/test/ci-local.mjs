/**
 * 本地 CI 聚合入口（与 .github/workflows/ci.yml 的检查步骤等价，便于本地复现）。
 * 用法：node scripts/test/ci-local.mjs [--quick]
 *   --quick：跳过覆盖率与冒烟（仅做格式/lint/测试/构建/门禁检查）。
 */
import path from "node:path";
import process from "node:process";
import { bin, pnpmCommand, repoRoot, run, summarize } from "./lib/exec.mjs";

const quick = process.argv.includes("--quick");
const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (args, options) => run(pnpm, [...pnpmPrefix, ...args], options);

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

record("cargo fmt --check", run(cargo, ["fmt", "--all", "--", "--check"]));
record(
  "cargo clippy --workspace -- -D warnings",
  run(cargo, ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
);
record("cargo check --workspace", run(cargo, ["check", "--workspace"]));
// 壳层 aether-tauri 的单元测试在 windows-gnu 下因 WebView2Loader/运行时差异无法运行；
// 壳层验证为构建+冒烟（smoke-desktop），覆盖率门禁同样排除该 crate。
record("cargo test --workspace（排除壳层）", run(cargo, ["test", "--workspace", "--exclude", "aether-tauri"]));
record("pnpm version:check", run(node, [path.join(repoRoot, "scripts", "ci", "version.mjs"), "check"]));
record("pnpm typecheck", pnpmRun(["-r", "--if-present", "typecheck"]));
record("pnpm test", pnpmRun(["-r", "--if-present", "test"]));
record("pnpm build", pnpmRun(["-r", "--if-present", "build"]));
record("相似度扫描", run(node, [path.join(repoRoot, "scripts", "ci", "similarity-scan.mjs"), "--root", "."]));
record("npm 许可证检查", run(node, [path.join(repoRoot, "scripts", "ci", "check-npm-licenses.mjs")]));
record("cargo deny check licenses/bans/sources", run(cargo, ["deny", "check", "licenses", "bans", "sources"]));
record("verify-version-sync", run(node, [path.join(repoRoot, "scripts", "test", "verify-version-sync.mjs")]));
record("verify-m1-02", run(node, [path.join(repoRoot, "scripts", "test", "m1-02", "verify-m1-02.mjs")]));
record("verify-m1-03", run(node, [path.join(repoRoot, "scripts", "test", "m1-03", "verify-m1-03.mjs")]));
record(
  "verify-m1-04（单写队列/group commit/读连接池；含 release 基准）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-04", "verify-m1-04.mjs")]),
);
record(
  "verify-m1-05（事件管线：sequencer/先日志后广播/delta/补读/降级与重试口径；含证据归档）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-05", "verify-m1-05.mjs")]),
);
record("verify-m1-07（密钥自检/脱敏/降级演练）", run(node, [path.join(repoRoot, "scripts", "test", "m1-07-verify.mjs")]));
record(
  "verify-m1-06（目录检测/迁移流/单实例；E2E 需真实 WebView，见 Windows CI）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-06", "verify-m1-06.mjs"), "--skip-e2e"]),
);
record(
  "verify-m1-08（壳与安全基线单测矩阵/静态检查；E2E 需真实 WebView，见 Windows CI）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-08", "verify-m1-08.mjs"), "--skip-e2e"]),
);
record(
  "verify-m1-09（线协议/Mock：握手/流式/中断/dispose/健壮性注入/2MiB 契约/吞吐基准；需 Bun，见 CI wire-protocol-mock job）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-09", "verify-m1-09.mjs")]),
);
record(
  "verify-m1-10（适配器监督器：状态机/心跳/进程组/台账三条件/终止序列/资源告警/retry·enable）",
  run(node, [path.join(repoRoot, "scripts", "test", "m1-10", "verify-m1-10.mjs")]),
);
record(
  "verify-m2-01（生命周期：状态机/run 串行/幂等重启重放/ack 快路径/120s 断流/runtime_* IPC）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-01", "verify-m2-01.mjs")]),
);
record(
  "verify-m2-02（首个真实适配器 Claude Code：一致性/工具事件/T5a 30s Ready+Mode R 重放；需 Bun）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-02", "verify-m2-02.mjs")]),
);
record(
  "verify-m2-03（权限网关：策略矩阵/T7 路径逃逸 100% deny+审计/300s 超时/T6 100 并发）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-03", "verify-m2-03.mjs")]),
);
record(
  "verify-m2-04（背压分级与 journal 补读：L2/L3/慢订阅者/Lagged/存储侧背压例外与隔离）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-04", "verify-m2-04.mjs")]),
);
record(
  "verify-m2-05（取消树与看门狗：20 会话取消风暴 ≤10s / interrupt 回 idle 可续聊 / 10s dump+强制清理）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-05", "verify-m2-05.mjs")]),
);
record(
  "verify-m2-06（关闭序列五步 shadow 日志 / -wal 0 字节无残留句柄 / checkpoint 退避与运行期阈值）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-06", "verify-m2-06.mjs")]),
);
record(
  "verify-m2-07（panic 隔离 / unwrap 静态 0 告警 / RSS 巡检与 delta 限流 / UI health 5s·15s / 日志汇聚端；E2E 需真实 WebView，见 Windows CI）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-07", "verify-m2-07.mjs"), "--skip-e2e"]),
);
record(
  "verify-m2-09（大行与 OOM 防护：>2MiB 不缓冲断连记错/1–2MiB 内存曲线受控/artifact_ref 附件仅存 artifacts 路径/1.5GiB 压力告警限流不崩溃；需 Bun + ~1.5GiB 内存）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-09", "verify-m2-09.mjs")]),
);
record(
  "verify-m2-11（三运行时适配器：Claude/Codex/DSH 单测+集成、DSH DoD1–8、静态合规；需 Bun；真实运行时为 opt-in）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-11", "verify-m2-11.mjs")]),
);
record(
  "verify-m2-10（权限回环：适配器文件类工具调用 100% 经 permission.request 回环、零直通；异常路径 deny/超时/once/session/重启 pending；证据可导出归档；需 Bun + 真实 Mock + 真实网关）",
  run(node, [path.join(repoRoot, "scripts", "test", "m2-10", "verify-m2-10.mjs")]),
);

if (!quick) {
  record("verify-coverage-gate", run(node, [path.join(repoRoot, "scripts", "test", "verify-coverage-gate.mjs")]));
  record("smoke-desktop", run(node, [path.join(repoRoot, "scripts", "test", "smoke-desktop.mjs")]));
}

process.exit(summarize("ci-local", checks));
