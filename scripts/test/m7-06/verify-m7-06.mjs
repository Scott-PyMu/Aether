#!/usr/bin/env node
/**
 * M7-06 验证：供应商消费映射、连通性测试与引用注入（ADR-024；DoD1–DoD8）。
 *
 * 覆盖：
 * - 静态登记：ADR-024 / 命令面与错误码（provider_test / provider_models_fetch /
 *   provider_unreachable / provider_auth_failed / provider_error）/ bindings 生成物 /
 *   env 消费映射常量 / 三适配器物化标记 / UI 锚点（`provider-test-result`、
 *   `provider-models-fetch`、`file-reference`）；
 * - DoD1/DoD2：`aether-adapters m7_06_provider_env`（spawn 注入点）+
 *   `aether-tauri m7_06_provider_env`（消费映射选择/裁剪/失败隔离）；
 * - DoD5：`aether-tauri m7_06_provider_connectivity`（五路径 + 无密钥不发请求 +
 *   幂等 upsert + 归一化）+ `provider_connectivity` 单元（端点推导/超时注入）；
 * - DoD4：`adapter-dsh dsh-model.test`（模型选项解析/选择）+ `dsh-adapter.test`（配置项下发）；
 * - DoD6：`adapter-sdk references.test`（引用块逐字形态）+ 四适配器（含 Mock 观测）+
 *   `aether-tauri ipc_validation`（`references` 畸形矩阵）；
 * - DoD7：桌面 UI（`m7_06_file_reference` + `m3_11_providers` 三态/获取模型）；
 * - DoD8 回归：`m3_11_providers`（供应商三项 + `providers_list` 边界）+
 *   `m3_06_retry` / `m7_02_gate` / `m7_03_pause`（引用线程化路径）+ `verify:m1-03`；
 * - 真实冒烟（opt-in `AETHER_M7_06_REAL=1`）：见 §实时段（缺凭证**显式 SKIP**）。
 *
 * 运行：node scripts/test/m7-06/verify-m7-06.mjs
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-06", `evidence-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });

const checks = [];
const skipped = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const recordSkip = (name, reason) => {
  skipped.push({ name, reason });
  console.log(`[m7-06] SKIP（显式）：${name} —— ${reason}`);
};

function vitest(packageName, files) {
  return run(pnpm, [...pnpmPrefix, "--filter", packageName, "exec", "vitest", "run", ...files]);
}

// ===== 静态登记与实现标记（任一缺失即失败） =====
try {
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  // ADR-024 登记（M5-01 批次合入稿）。
  const adr = read("docs/adr/ADR-024-provider-consumption-and-references.md");
  for (const token of [
    "provider_test",
    "provider_models_fetch",
    "provider_auth_failed",
    "provider_unreachable",
    "provider_error",
    "session.send.references",
    "aether-references",
  ]) {
    assert(adr.includes(token), `ADR-024 缺少登记项：${token}`);
  }

  // 连通性实现（超时常量 + 五路径映射 + 端点推导）。
  const connectivity = read("crates/aether-app/src/provider_connectivity.rs");
  for (const token of [
    "PROVIDER_HTTP_TOTAL_TIMEOUT",
    "PROVIDER_HTTP_CONNECT_TIMEOUT",
    "models_endpoint",
    "provider_auth_failed",
    "provider_unreachable",
    "provider_error",
    "normalize_model_ids",
    // TLS 链路：平台根证书验证器（OS 信任库；不引入 CDLA 许可的 webpki-roots）。
    "RootCerts::PlatformVerifier",
  ]) {
    assert(connectivity.includes(token), `provider_connectivity.rs 缺少：${token}`);
  }
  const tauriCargo = read("crates/aether-tauri/Cargo.toml");
  assert(tauriCargo.includes("platform-verifier"), "Cargo.toml 缺少 ureq platform-verifier 特性");
  assert(
    !/^\s*webpki-roots\s*=/m.test(tauriCargo),
    "Cargo.toml 不得声明 webpki-roots 依赖（许可清单外）",
  );

  // DoD1 实测脚本与连通性 TLS 实测入口存在（登记硬校验）。
  for (const script of [
    "scripts/test/m7-06/recording-proxy.mjs",
    "scripts/test/m7-06/claude-projection-probe.mjs",
    "scripts/test/m7-06/dsh-config-probe.mjs",
  ]) {
    assert(existsSync(path.join(repoRoot, script)), `实测脚本缺失：${script}`);
  }
  const connectivityTests = read("crates/aether-tauri/tests/m7_06_provider_connectivity.rs");
  assert(
    connectivityTests.includes("real_https_tls_path_provider_test"),
    "缺少 TLS 链路实测入口（m7_06_provider_connectivity）",
  );

  // 命令实现（幂等 upsert / 无密钥不发请求 / AETHER 辅助 env 常量）。
  const providerControl = read("crates/aether-app/src/provider_control.rs");
  for (const token of [
    "pub fn provider_test",
    "pub fn provider_models_fetch",
    "RefreshProviderModelDisplayName",
    "api_key_ref",
    "spawn_blocking",
  ]) {
    assert(providerControl.includes(token), `provider_control.rs 缺少：${token}`);
  }

  // 消费映射 hook（运行时映射 + env 常量 + 选择规则）。
  const providerEnv = read("crates/aether-app/src/provider_env.rs");
  for (const token of [
    "expected_provider_type",
    "ENV_ANTHROPIC_API_KEY",
    "ENV_OPENAI_API_KEY",
    "ENV_DEEPSEEK_API_KEY",
    "ENV_CODEX_HOME",
    "ENV_AETHER_PROVIDER_BASE_URL",
    "ENV_AETHER_PROVIDER_MODELS",
  ]) {
    assert(providerEnv.includes(token), `provider_env.rs 缺少：${token}`);
  }

  // spawn 注入点（aether-adapters）。
  const providerEnvTrait = read("crates/aether-adapters/src/supervisor/provider_env.rs");
  assert(providerEnvTrait.includes("SpawnEnvSource"), "aether-adapters 缺少 SpawnEnvSource");
  const runtimeRs = read("crates/aether-adapters/src/supervisor/runtime.rs");
  assert(runtimeRs.includes("env_source"), "runtime.rs 未接线 env_source");

  // 命令面与错误码登记（含 bindings 生成物 token）。
  const errorRs = read("crates/aether-app/src/ipc/error.rs");
  for (const token of ["ProviderUnreachable", "ProviderAuthFailed", "ProviderError"]) {
    assert(errorRs.includes(token), `error.rs 缺少：${token}`);
  }
  const commandsRs = read("crates/aether-tauri/src/ipc/commands.rs");
  for (const token of ["provider_test", "provider_models_fetch"]) {
    assert(commandsRs.includes(token), `commands.rs 缺少：${token}`);
  }
  const bindings = read("packages/protocol/src/bindings.ts");
  for (const token of [
    "providerTest",
    "providerModelsFetch",
    "references",
    "provider_unreachable",
    "provider_auth_failed",
    "provider_error",
  ]) {
    assert(bindings.includes(token), `bindings.ts 缺少生成物 token：${token}`);
  }

  // 引用注入映射（SDK + 四适配器 + 前端锚点）。
  const referencesTs = read("packages/adapter-sdk/src/references.ts");
  for (const token of [
    "REFERENCE_BLOCK_OPEN",
    "REFERENCE_BLOCK_CLOSE",
    "REFERENCE_BLOCK_INSTRUCTION",
    "composePromptWithReferences",
  ]) {
    assert(referencesTs.includes(token), `references.ts 缺少：${token}`);
  }
  for (const file of [
    "packages/adapter-claude-code/src/claude-adapter.ts",
    "packages/adapter-codex/src/codex-adapter.ts",
    "packages/adapter-dsh/src/dsh-adapter.ts",
  ]) {
    const text = read(file);
    assert(text.includes("composePromptWithReferences"), `${file} 未接入引用块映射`);
  }
  const mockAdapter = read("packages/adapter-mock/src/mock-adapter.ts");
  assert(mockAdapter.includes("has_references"), "mock-adapter 缺少引用观测字段");
  const codexCli = read("packages/adapter-codex/src/codex-cli.ts");
  assert(codexCli.includes("CODEX_CONFIG_MARKER"), "codex-cli 缺少 config.toml 物化标记");
  const dshModel = read("packages/adapter-dsh/src/dsh-model.ts");
  assert(dshModel.includes("providerFromEnv"), "dsh-model 缺少消费映射构造");
  const providersPage = read("apps/desktop/src/ProvidersPage.tsx");
  for (const token of ["provider-test-result", "provider-models-fetch"]) {
    assert(providersPage.includes(token), `ProvidersPage 缺少锚点：${token}`);
  }
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  for (const token of ["file-reference", "file-reference-chip", "pendingReferences"]) {
    assert(workbench.includes(token), `SessionWorkbench 缺少引用注入实现：${token}`);
  }

  console.log("[static] 静态登记与实现标记：通过");
  record("静态：ADR-024 / 命令面 / 错误码 / bindings / env / 适配器 / UI 标记", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record(`静态：${error.message}`, 1);
}

// ===== Rust 单元与集成 =====
record("aether-core --lib（MessageReference/ReferenceKind）", run(cargo, ["test", "-p", "aether-core", "--lib"]));
record(
  "aether-adapters --lib（方法表/客户端 references 序列化）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]),
);
record(
  "DoD1/2 aether-adapters m7_06_provider_env（spawn 注入点 2 用例）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m7_06_provider_env"]),
);
record(
  "aether-store --lib + m3_11_providers（幂等 upsert 存储面）",
  run(cargo, ["test", "-p", "aether-store", "--lib"]),
);
record(
  "DoD8 aether-store m3_11_providers（刷新 display_name 存储断言）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m3_11_providers"]),
);
record(
  "aether-control --lib（references 线程化编译 + 单测）",
  run(cargo, ["test", "-p", "aether-control", "--lib"]),
);
record(
  "aether-tauri --lib（provider_connectivity 单元 8 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--lib"]),
);
record(
  "DoD5 aether-tauri m7_06_provider_connectivity（五路径 8 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_06_provider_connectivity"]),
);
record(
  "DoD1/2 aether-tauri m7_06_provider_env（选择/裁剪/失败隔离 5 用例）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_06_provider_env"]),
);
record(
  "DoD8 aether-tauri m3_11_providers（密钥三态/内置约束/命令矩阵；upsert 断言更新）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "m3_11_providers"]),
);
record(
  "DoD6 aether-tauri ipc_validation（references/连通性畸形矩阵）",
  run(cargo, ["test", "-p", "aether-tauri", "--test", "ipc_validation"]),
);

// 引用线程化的门禁/等待/重放路径回归（需要真实夹具）。
record(
  "构建 aether-adapter-fixture",
  run(cargo, ["build", "-p", "aether-adapters", "--bin", "aether-adapter-fixture"]),
);
if (!existsSync(fixture)) {
  record(`夹具产物缺失：${path.relative(repoRoot, fixture)}`, 1);
} else {
  const fixtureEnv = { ...process.env, AETHER_FIXTURE_BIN: fixture, AETHER_REQUIRE_FIXTURE: "1" };
  record(
    "回归 aether-control m2_01_lifecycle（10 用例）",
    run(cargo, ["test", "-p", "aether-control", "--test", "m2_01_lifecycle"]),
  );
  record(
    "回归 aether-control m3_06_retry（重放：不带引用内存态）",
    run(cargo, ["test", "-p", "aether-control", "--test", "m3_06_retry"]),
  );
  record(
    "回归 aether-tauri m7_02_gate（闸门 FIFO/挂起；引用于活跃位/等待槽）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_02_gate"], { env: fixtureEnv }),
  );
  record(
    "回归 aether-tauri m7_03_pause（暂停重放路径）",
    run(cargo, ["test", "-p", "aether-tauri", "--test", "m7_03_pause"], { env: fixtureEnv }),
  );
}

// ===== TS 适配器与 SDK =====
record(
  "DoD6 adapter-sdk references（引用块逐字形态/解析宽容）",
  vitest("@aether/adapter-sdk", ["src/references.test.ts"]),
);
record(
  "DoD1 adapter-claude-code（cli 投影 + 引用块 + 端到端）",
  vitest("@aether/adapter-claude-code", ["src/cli.test.ts", "src/claude-adapter.test.ts"]),
);
record(
  "DoD1 adapter-codex（cli 投影/物化 + 引用块 + 端到端）",
  vitest("@aether/adapter-codex", ["src/cli.test.ts", "src/codex-adapter.test.ts"]),
);
record(
  "DoD4 adapter-dsh（模型选项/消费映射 patch + 引用块）",
  vitest("@aether/adapter-dsh", [
    "src/dsh-model.test.ts",
    "src/dsh-plugin.test.ts",
    "src/dsh-adapter.test.ts",
  ]),
);
record(
  "DoD6 adapter-mock（references 观测）",
  vitest("@aether/adapter-mock", ["src/mock-adapter.test.ts"]),
);

// ===== 前端 =====
record(
  "DoD7 UI m7_06_file_reference（@/移除/发送携带/清空 4 用例）",
  vitest("@aether/desktop", ["src/m7_06_file_reference.test.tsx"]),
);
record(
  "DoD7/8 UI m3_11_providers（真实测试连接三态/获取模型/回归）",
  vitest("@aether/desktop", ["src/m3_11_providers.test.tsx"]),
);
record("前端 typecheck", run(pnpm, [...pnpmPrefix, "--filter", "@aether/desktop", "typecheck"]));

// ===== 迁移 ↔ 附录 C 双向比对（M7-06 无迁移） =====
record(
  "回归 verify:m1-03（迁移 ↔ 附录 C 双向比对；M7-06 无迁移）",
  run(process.execPath, [path.join("scripts", "test", "m1-03", "verify-m1-03.mjs")], {
    cwd: repoRoot,
  }),
);

// ===== 真实冒烟（opt-in；缺凭证显式 SKIP，禁止静默跳过） =====
const realEnabled = process.env.AETHER_M7_06_REAL === "1";
if (!realEnabled) {
  recordSkip(
    "真实消费冒烟（DoD3）",
    "未设置 AETHER_M7_06_REAL=1（opt-in；本地跑法见证据文档）",
  );
} else {
  const bundleDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-06", "bundle");
  mkdirSync(bundleDir, { recursive: true });
  const bunBin = process.env.AETHER_BUN ?? path.join(process.env.USERPROFILE ?? "", ".bun", "bin", "bun.exe");
  const builds = [
    ["claude", "packages/adapter-claude-code/src/main.ts", `aether-claude-adapter${exeSuffix}`],
    ["codex", "packages/adapter-codex/src/main.ts", `aether-codex-adapter${exeSuffix}`],
    ["dsh", "packages/adapter-dsh/src/main.ts", `aether-dsh-adapter${exeSuffix}`],
  ];
  let bundleOk = true;
  for (const [label, entry, outfile] of builds) {
    const result = run(bunBin, ["build", entry, "--compile", "--outfile", path.join(bundleDir, outfile)], {
      cwd: repoRoot,
    });
    if (result !== 0) {
      bundleOk = false;
      record(`真实冒烟：构建 ${label} 适配器 bundle`, result);
    }
  }
  if (!bundleOk) {
    record("真实冒烟：适配器 bundle 构建失败（opt-in 场景不得跳过）", 1);
  } else {
    const parseEnvFile = (file, { keyPattern, urlLine }) => {
      const filePath = path.join(repoRoot, file);
      if (!existsSync(filePath)) return null;
      const text = readFileSync(filePath, "utf8");
      const lines = text
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter((line) => line.length > 0);
      const keyLine = lines.find((line) => keyPattern.test(line));
      const url = lines.find((line) => urlLine.test(line))?.match(/https?:\/\/\S+/)?.[0];
      const model = lines.find((line) => /^model:/i.test(line))?.split(":").slice(1).join(":").trim();
      const token = keyLine?.split(":").slice(1).join(":").trim() || keyLine?.replace(/^[^:]*:/, "").trim();
      if (!url || !token) return null;
      return { url, token, model };
    };
    const creds = {
      claude:
        process.env.AETHER_M7_06_CLAUDE_UPSTREAM && process.env.AETHER_M7_06_CLAUDE_TOKEN
          ? {
              url: process.env.AETHER_M7_06_CLAUDE_UPSTREAM,
              token: process.env.AETHER_M7_06_CLAUDE_TOKEN,
              model: process.env.AETHER_M7_06_CLAUDE_MODEL ?? "deepseek-v4-pro",
            }
          : parseEnvFile("claud_test_env.txt", { keyPattern: /^api-?key/i, urlLine: /https?:\/\//i }),
      dsh:
        process.env.AETHER_M7_06_DSH_UPSTREAM && process.env.AETHER_M7_06_DSH_TOKEN
          ? {
              url: process.env.AETHER_M7_06_DSH_UPSTREAM,
              token: process.env.AETHER_M7_06_DSH_TOKEN,
              model: process.env.AETHER_M7_06_DSH_MODEL ?? "deepseek-v4.1-flash",
            }
          : parseEnvFile("dsh_env.txt", { keyPattern: /^api-?key/i, urlLine: /url:/i }),
      codex:
        process.env.AETHER_M7_06_CODEX_UPSTREAM && process.env.AETHER_M7_06_CODEX_TOKEN
          ? {
              url: process.env.AETHER_M7_06_CODEX_UPSTREAM,
              token: process.env.AETHER_M7_06_CODEX_TOKEN,
              model: process.env.AETHER_M7_06_CODEX_MODEL ?? "deepseek-v4-pro",
            }
          : null,
    };
    // 模型显式覆盖（网关支持的模型名可能与本地凭据文件口径不同；`AETHER_M7_06_<R>_MODEL` 优先）。
    if (creds.claude && process.env.AETHER_M7_06_CLAUDE_MODEL) {
      creds.claude.model = process.env.AETHER_M7_06_CLAUDE_MODEL;
    }
    if (creds.dsh && process.env.AETHER_M7_06_DSH_MODEL) {
      creds.dsh.model = process.env.AETHER_M7_06_DSH_MODEL;
    }
    // Codex 回退：同网关的 OpenAI 面（streamax 口径：claude 面 `/anth/...` → OpenAI 面 `/open/v1`；
    // 实测 `POST /open/v1/responses` 支持 Responses API）。
    if (!creds.codex && creds.claude) {
      const claudeUrl = new URL(creds.claude.url);
      const basePath = claudeUrl.pathname.replace(/\/+$/, "");
      if (basePath.startsWith("/anth")) {
        creds.codex = {
          url: `${claudeUrl.origin}/open/v1`,
          token: process.env.AETHER_M7_06_CODEX_TOKEN ?? creds.claude.token,
          model: process.env.AETHER_M7_06_CODEX_MODEL ?? creds.claude.model,
        };
      } else {
        recordSkip(
          "真实冒烟：codex（自动回退）",
          `claude 凭据路径为 ${basePath || "/"}（非 streamax `/anth` 口径）；请显式提供 AETHER_M7_06_CODEX_UPSTREAM/_TOKEN`,
        );
      }
    }
    const dshBin =
      process.env.AETHER_M7_06_DSH_BIN ??
      (process.env.APPDATA
        ? path.join(
            process.env.APPDATA,
            "npm",
            "node_modules",
            "@deepseek-ai",
            "dsh",
            "lib",
            "bin.js",
          )
        : null);

    const runReal = (runtime, testName, env) => {
      const result = run(
        cargo,
        ["test", "-p", "aether-tauri", "--test", "m7_06_consumption", "--", "--ignored", "--nocapture", testName],
        { env },
      );
      record(`真实冒烟：${runtime}（配置供应商 → run 使用 base URL/模型）`, result);
      return result === 0;
    };
    /** 上游 URL → 代理源 + API 路径前缀（前缀保留在 provider.base_url 上）。 */
    const splitUpstream = (url) => {
      const parsed = new URL(url);
      return { origin: parsed.origin, prefix: parsed.pathname.replace(/\/+$/, "") };
    };

    if (creds.claude) {
      // Claude Code 自身会追加 `/v1/messages`；当凭据 URL 以 `/v1` 结尾时前缀须回落一级
      // （实测：base `.../anth/v1` 的实际端点为 `.../anth/v1/messages`）。
      const upstream = splitUpstream(creds.claude.url);
      const claudePrefix = upstream.prefix.endsWith("/v1")
        ? upstream.prefix.slice(0, -3)
        : upstream.prefix;
      runReal("claude-code", "real_claude_code_consumption", {
        ...process.env,
        AETHER_M7_06_REAL: "1",
        AETHER_M7_06_CLAUDE_UPSTREAM: upstream.origin,
        AETHER_M7_06_CLAUDE_PREFIX: claudePrefix,
        AETHER_M7_06_CLAUDE_TOKEN: creds.claude.token,
        AETHER_M7_06_CLAUDE_MODEL: creds.claude.model,
        AETHER_M7_06_EVIDENCE_DIR: path.join(evidenceDir, "real-claude-code"),
        AETHER_CLAUDE_ADAPTER: path.join(bundleDir, `aether-claude-adapter${exeSuffix}`),
      });
    } else {
      recordSkip("真实冒烟：claude-code", "未提供 AETHER_M7_06_CLAUDE_UPSTREAM/_TOKEN（或 claud_test_env.txt）");
    }
    if (creds.codex) {
      const upstream = splitUpstream(creds.codex.url);
      runReal("codex", "real_codex_consumption", {
        ...process.env,
        AETHER_M7_06_REAL: "1",
        AETHER_M7_06_CODEX_UPSTREAM: upstream.origin,
        AETHER_M7_06_CODEX_PREFIX: upstream.prefix,
        AETHER_M7_06_CODEX_TOKEN: creds.codex.token,
        AETHER_M7_06_CODEX_MODEL: creds.codex.model,
        AETHER_M7_06_EVIDENCE_DIR: path.join(evidenceDir, "real-codex"),
        AETHER_CODEX_ADAPTER: path.join(bundleDir, `aether-codex-adapter${exeSuffix}`),
      });
    } else {
      recordSkip("真实冒烟：codex", "未提供 AETHER_M7_06_CODEX_UPSTREAM/_TOKEN（网关需支持 Responses API）");
    }
    if (creds.dsh && dshBin && existsSync(dshBin)) {
      const upstream = splitUpstream(creds.dsh.url);
      runReal("deepseek-harness", "real_dsh_consumption", {
        ...process.env,
        AETHER_M7_06_REAL: "1",
        AETHER_M7_06_DSH_UPSTREAM: upstream.origin,
        AETHER_M7_06_DSH_PREFIX: upstream.prefix,
        AETHER_M7_06_DSH_TOKEN: creds.dsh.token,
        AETHER_M7_06_DSH_MODEL: creds.dsh.model,
        AETHER_M7_06_DSH_BIN: dshBin,
        AETHER_M7_06_EVIDENCE_DIR: path.join(evidenceDir, "real-deepseek-harness"),
        AETHER_DSH_ADAPTER: path.join(bundleDir, `aether-dsh-adapter${exeSuffix}`),
      });
    } else {
      recordSkip(
        "真实冒烟：deepseek-harness",
        "未提供 AETHER_M7_06_DSH_UPSTREAM/_TOKEN 或未安装 @deepseek-ai/dsh",
      );
    }

    // ===== DoD1 实测：档位投影等价性（ADR-024 附录 A-4） =====
    if (creds.claude) {
      record(
        "DoD1 实测：Claude acceptEdits 工作区边界（区内允许/区外拒绝；顶档放行；只读拒绝）",
        run(process.execPath, [path.join("scripts", "test", "m7-06", "claude-projection-probe.mjs")], {
          cwd: repoRoot,
          env: {
            ...process.env,
            AETHER_M7_06_CLAUDE_UPSTREAM: creds.claude.url,
            AETHER_M7_06_CLAUDE_TOKEN: creds.claude.token,
            AETHER_M7_06_CLAUDE_MODEL: creds.claude.model,
            AETHER_M7_06_EVIDENCE_DIR: path.join(evidenceDir, "projection"),
          },
        }),
      );
    } else {
      recordSkip("DoD1 实测：Claude 投影等价性", "缺少 claude 真实凭据（同真实冒烟口径）");
    }
    const dshProbeBin = dshBin && existsSync(dshBin) ? dshBin : null;
    if (dshProbeBin) {
      record(
        "DoD1 实测：DSH headless preset 等价性（configOptions 面探测；不等价 → 降级登记）",
        run(process.execPath, [path.join("scripts", "test", "m7-06", "dsh-config-probe.mjs")], {
          cwd: repoRoot,
          env: {
            ...process.env,
            AETHER_M7_06_DSH_BIN: dshProbeBin,
            AETHER_M7_06_EVIDENCE_DIR: path.join(evidenceDir, "projection"),
          },
        }),
      );
    } else {
      recordSkip("DoD1 实测：DSH 投影等价性", "未安装 @deepseek-ai/dsh");
    }

    // ===== DoD5 实测：真实 HTTPS TLS 链路（ureq + rustls + 平台验证器） =====
    if (creds.claude) {
      record(
        "DoD5 实测：真实 HTTPS TLS 链路（/models 404 → provider_error 含状态码；不得 unreachable）",
        run(
          cargo,
          [
            "test",
            "-p",
            "aether-tauri",
            "--test",
            "m7_06_provider_connectivity",
            "--",
            "--ignored",
            "--nocapture",
            "real_https_tls_path_provider_test",
          ],
          {
            env: {
              ...process.env,
              AETHER_M7_06_REAL: "1",
              AETHER_M7_06_TLS_BASE_URL: creds.claude.url.replace(/\/+$/, ""),
              AETHER_M7_06_TLS_TOKEN: creds.claude.token,
            },
          },
        ),
      );
    } else {
      recordSkip("DoD5 实测：真实 HTTPS TLS 链路", "缺少 claude 真实凭据（同真实冒烟口径）");
    }
  }
}

// ===== 证据归档 =====
const summary = {
  task: "M7-06",
  stamp,
  platform: process.platform,
  evidence_dir: path.relative(repoRoot, evidenceDir),
  checks,
  skipped,
};
writeFileSync(
  path.join(evidenceDir, "summary.json"),
  `${JSON.stringify(summary, null, 2)}\n`,
  "utf8",
);
console.log(`[m7-06] 证据目录：${evidenceDir}`);
if (skipped.length > 0) {
  console.log(`[m7-06] 显式 SKIP ${skipped.length} 项（见 summary.json.skipped）`);
}

process.exit(summarize("verify-m7-06", checks));
