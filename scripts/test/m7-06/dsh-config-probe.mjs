#!/usr/bin/env node
/**
 * M7-06/ADR-024 DoD1 实测：DSH「headless preset 等价性」核验（只读探针）。
 *
 * 口径：以真实 DSH（`--profile acp`）会话面为事实源——`session/new` 返回的
 * `configOptions` 是 ACP 暴露的全部按会话配置面。若不存在 file policy / approval
 * 类配置项，则档位无法投影为 DSH 运行时侧参数 → 判定**不等价**（ADR-014 §6-4：
 * 该运行时对应档位置灰/降级并登记；禁止静默按部分等价实现）。档位约束仍由 D9
 * 网关矩阵执行（DSH 工具调用 100% 经 `permission.request` 回环）。
 *
 * 依赖（opt-in；缺任一 → 显式 SKIP + 退出码 3）：
 *   AETHER_M7_06_DSH_BIN（缺省自动探测 %APPDATA%/npm/node_modules/@deepseek-ai/dsh/lib/bin.js）
 * 产物：`<evidence>/dsh-config-probe.json`。
 * 运行：node scripts/test/m7-06/dsh-config-probe.mjs
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

import { repoRoot } from "../lib/exec.mjs";

const { join } = path;

const dshBin =
  process.env.AETHER_M7_06_DSH_BIN ??
  (process.env.APPDATA
    ? path.join(process.env.APPDATA, "npm", "node_modules", "@deepseek-ai", "dsh", "lib", "bin.js")
    : "");
if (!dshBin || !existsSync(dshBin)) {
  console.log("SKIP（显式）：dsh-config-probe 未找到 @deepseek-ai/dsh（AETHER_M7_06_DSH_BIN）");
  process.exit(3);
}
const nodeBin = process.env.AETHER_M7_06_DSH_NODE ?? process.execPath;
const evidenceDir =
  process.env.AETHER_M7_06_EVIDENCE_DIR ??
  path.join(repoRoot, "scripts", "test", ".tmp", "m7-06", "dsh-config-probe");
mkdirSync(evidenceDir, { recursive: true });
const home = path.join(evidenceDir, "dsh-home");
mkdirSync(join(home, "workspace"), { recursive: true });

const child = spawn(nodeBin, [dshBin, "--profile", "acp"], {
  env: { ...process.env, DSH_HOME: home },
  stdio: ["pipe", "pipe", "pipe"],
  windowsHide: true,
});
let nextId = 1;
const pending = new Map();
function request(method, params, timeoutMs = 60000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${method} 超时`)), timeoutMs);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  });
}
readline.createInterface({ input: child.stdout }).on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id !== undefined && pending.has(message.id)) {
    const entry = pending.get(message.id);
    pending.delete(message.id);
    clearTimeout(entry.timer);
    if (message.error) entry.reject(new Error(JSON.stringify(message.error)));
    else entry.resolve(message.result);
    return;
  }
  if (message.method === "session/request_permission") {
    child.stdin.write(
      `${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: { outcome: { outcome: "approved" } } })}\n`,
    );
  }
});

let verdict;
try {
  await request("initialize", {
    protocolVersion: 1,
    clientCapabilities: {},
    clientInfo: { name: "aether-m7-06-probe", version: "0.0.1" },
  });
  const created = await request("session/new", { cwd: join(home, "workspace"), mcpServers: [] });
  const options = Array.isArray(created.configOptions) ? created.configOptions : [];
  const ids = options.map((option) => option.id);
  const filePolicyLike = options.filter((option) =>
    /(sandbox|file|policy|approval|permission)/i.test(String(option.id ?? "") + String(option.category ?? "")),
  );
  verdict = {
    task: "M7-06",
    probe: "dsh-config-probe",
    dsh_bin: dshBin.replace(/\\/g, "/"),
    checked_at: new Date().toISOString(),
    config_option_ids: ids,
    file_policy_or_approval_options: filePolicyLike.map((option) => option.id),
    equivalent: filePolicyLike.length > 0,
    conclusion: filePolicyLike.length > 0 ? "存在文件策略/审批配置面（需进一步逐档断言）" : "不等价：ACP 配置面无 file policy / approval 项 → 档位不投影（降级登记；D9 网关矩阵仍为权威）",
  };
} catch (error) {
  verdict = {
    task: "M7-06",
    probe: "dsh-config-probe",
    error: String(error).slice(0, 400),
    equivalent: false,
    conclusion: "探针失败（按不等价处理并排查）",
  };
}
writeFileSync(path.join(evidenceDir, "dsh-config-probe.json"), `${JSON.stringify(verdict, null, 2)}\n`, "utf8");
console.log(JSON.stringify(verdict, null, 2));
try {
  child.stdin.end();
} catch {
  /* 已关闭 */
}
setTimeout(() => {
  try {
    child.kill();
  } catch {
    /* 已退出 */
  }
  process.exit(verdict.error ? 1 : 0);
}, 1200);
