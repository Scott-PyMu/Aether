/**
 * M1-07 密钥存储与脱敏验证脚本（DoD 逐条证据）。
 *
 * 覆盖：
 *   1) DoD1：keychain 写→读→删自检（真实 OS 凭据库；Linux 无 Secret Service 时按设计降级）；
 *   2) DoD2：`sk-` / `eyJ` / PEM 三类真实样本经脱敏后，日志与诊断导出独立扫描 0 命中；
 *   3) DoD3：强制降级路径启动，展示「安全级别：降级」，明文不落盘、不回显；
 *   4) AGENTS §2.9：aether-security 源文件密钥形态扫描 0 命中（测试夹具不落密钥）。
 *
 * 样本策略（AGENTS.md §2.9）：三类样本运行期随机生成、真实结构/长度；
 * 原始样本只驻留内存，证据目录只写脱敏产物与扫描计数。
 *
 * 用法：node scripts/test/m1-07-verify.mjs
 * 退出码：0 = 全部通过；1 = 存在失败项（skip 不算失败）。
 */
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, summarize } from "./lib/exec.mjs";

const cargo = bin("cargo");
const outDir = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m1-07",
  new Date().toISOString().replace(/[:.]/g, "-"),
);
mkdirSync(outDir, { recursive: true });
console.log(`[m1-07] 证据目录：${path.relative(repoRoot, outDir)}`);

const checks = [];
const pass = (name) => checks.push({ name, expect: 0, exit: 0 });
const fail = (name, reason) => checks.push({ name, expect: 0, exit: 1, reason });
const skip = (name, reason) => checks.push({ name, skip: true, reason });

function cargoRun(args, options = {}) {
  const result = spawnSync(cargo, args, {
    cwd: repoRoot,
    encoding: "utf8",
    input: options.input,
    env: { ...process.env, ...(options.env ?? {}) },
    maxBuffer: 64 * 1024 * 1024,
  });
  return {
    exit: result.status ?? 1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
  };
}

// ---------------------------------------------------------------------------
// 独立扫描器（与 Rust 脱敏实现分离实现，避免同源漏检）
// ---------------------------------------------------------------------------
const SCAN_PATTERNS = {
  "sk-": /sk-[A-Za-z0-9_-]{8,}/g,
  eyJ: /eyJ[A-Za-z0-9_-]{8,}/g,
  PEM: /-----BEGIN[ A-Z0-9]*PRIVATE KEY/g,
};

function countHits(text) {
  const counts = {};
  for (const [name, pattern] of Object.entries(SCAN_PATTERNS)) {
    counts[name] = (text.match(pattern) ?? []).length;
  }
  return counts;
}

/**
 * §2.9 源文件扫描按「完整密钥形态」阈值：真实密钥长度均 ≥40 字符；
 * 源码中的格式串/装甲前缀（如 `sk-ant-api03-{}`、`{armor}`）不是密钥值，须避免误报。
 */
const SOURCE_SCAN_PATTERNS = {
  "sk-": /sk-[A-Za-z0-9_-]{24,}/g,
  eyJ: /eyJ[A-Za-z0-9_-]{24,}/g,
  "PEM-body": /-----BEGIN[ A-Z0-9]*PRIVATE KEY-----[ \t]*\r?\n[A-Za-z0-9+/=]{40,}/g,
};

function allZero(counts) {
  return Object.values(counts).every((count) => count === 0);
}

const b64url = (buffer) => buffer.toString("base64url");

function sampleApiKey() {
  return `sk-ant-api03-${b64url(crypto.randomBytes(71))}`;
}

function sampleJwt() {
  const header = b64url(Buffer.from(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const payload = b64url(
    Buffer.from(JSON.stringify({ sub: b64url(crypto.randomBytes(12)), iat: 1757894400 })),
  );
  return [header, payload, b64url(crypto.randomBytes(32))].join(".");
}

function samplePem(armor) {
  const encoded = crypto.randomBytes(1216).toString("base64");
  const body = encoded.match(/.{1,64}/g).join("\n");
  return `-----BEGIN ${armor}-----\n${body}\n-----END ${armor}-----`;
}

// ---------------------------------------------------------------------------
// Step 1：Rust 单元/集成测试（DoD1/2/3 的自动化断言）
// ---------------------------------------------------------------------------
{
  const tests = cargoRun(["test", "-p", "aether-security", "--", "--nocapture"]);
  const combined = `${tests.stdout}\n${tests.stderr}`;
  writeFileSync(path.join(outDir, "cargo-test.txt"), combined);
  const ok = tests.exit === 0 && combined.includes("test result: ok");
  if (ok) pass("DoD1/2/3 cargo test -p aether-security 全绿（含 0 命中扫描断言）");
  else fail("cargo test -p aether-security", `exit=${tests.exit}`);
}

// ---------------------------------------------------------------------------
// Step 2：脱敏器端到端（日志 + 诊断导出）
// ---------------------------------------------------------------------------
{
  const apiKey = sampleApiKey();
  const jwt = sampleJwt();
  const pem = samplePem("PRIVATE KEY");
  const pemCrlf = samplePem("OPENSSH PRIVATE KEY").replace(/\n/g, "\r\n");

  const corpus = [
    "2026-09-15T10:00:00.123Z INFO runtime.spawn adapter=codex",
    `2026-09-15T10:00:01.000Z DEBUG request headers authorization=Bearer ${jwt}`,
    `2026-09-15T10:00:02.000Z ERROR provider init failed: credentials rejected (key=${apiKey})`,
    "2026-09-15T10:00:03.000Z WARN adapter started pid=4242 latency_ms=87",
    `2026-09-15T10:00:04.000Z ERROR crash dump follows\r\n${pemCrlf}\r\n`,
    `2026-09-15T10:00:05.000Z INFO stdout scan begin\n${pem}`,
    "2026-09-15T10:00:06.000Z INFO health check ok",
  ].join("\n");

  const redactLog = cargoRun(
    ["run", "--quiet", "-p", "aether-security", "--example", "redact_filter", "--", "--mode", "text"],
    { input: corpus },
  );
  const rawHits = countHits(corpus);
  const redactedHits = countHits(redactLog.stdout);
  writeFileSync(path.join(outDir, "redacted-sample-log.log"), redactLog.stdout);

  const rawDetected =
    rawHits["sk-"] >= 1 && rawHits.eyJ >= 1 && rawHits.PEM >= 2;
  if (redactLog.exit === 0 && rawDetected) {
    pass(`DoD2 原始日志三类样本被独立扫描器命中（sk-=${rawHits["sk-"]}, eyJ=${rawHits.eyJ}, PEM=${rawHits.PEM}）`);
  } else {
    fail("DoD2 原始样本扫描（防假阴性）", `exit=${redactLog.exit} hits=${JSON.stringify(rawHits)}`);
  }

  if (allZero(redactedHits)) {
    pass(`DoD2 脱敏后日志 0 命中（sk-=${redactedHits["sk-"]}, eyJ=${redactedHits.eyJ}, PEM=${redactedHits.PEM}）`);
  } else {
    fail("DoD2 脱敏后日志 0 命中", JSON.stringify(redactedHits));
  }

  const noPlaintext =
    !redactLog.stdout.includes(apiKey) &&
    !redactLog.stdout.includes(jwt) &&
    !redactLog.stdout.includes(pem);
  if (noPlaintext) {
    pass("DoD2 脱敏后日志不含三类原文样本");
  } else {
    fail("DoD2 脱敏后日志仍含原文");
  }

  const benignKept =
    redactLog.stdout.includes("adapter started pid=4242") &&
    redactLog.stdout.includes("health check ok");
  if (benignKept) pass("DoD2 普通日志行未被误伤");
  else fail("DoD2 普通日志行被误伤");

  const bundle = {
    app: { name: "Aether", version: "0.1.0" },
    security: { level: "安全级别：系统凭据库", refs: ["keychain://aether/adapter-codex/api-key"] },
    config: { providers: { anthropic: { api_key: apiKey, base_url: "https://api.anthropic.com" } } },
    logs: [`Authorization: Bearer ${jwt}`, `private material:\n${pem}`],
    db: { size_bytes: 4096, events: 120 },
  };
  const rawExport = JSON.stringify(bundle, null, 2);
  const redactExport = cargoRun(
    ["run", "--quiet", "-p", "aether-security", "--example", "redact_filter", "--", "--mode", "json"],
    { input: rawExport },
  );
  writeFileSync(path.join(outDir, "redacted-diagnostics-export.json"), redactExport.stdout);

  const rawExportHits = countHits(rawExport);
  const redactedExportHits = countHits(redactExport.stdout);
  const exportRawDetected =
    rawExportHits["sk-"] >= 1 && rawExportHits.eyJ >= 1 && rawExportHits.PEM >= 1;
  if (redactExport.exit === 0 && exportRawDetected) {
    pass(`DoD2 原始诊断导出三类样本可被扫描命中（sk-=${rawExportHits["sk-"]}, eyJ=${rawExportHits.eyJ}, PEM=${rawExportHits.PEM}）`);
  } else {
    fail("DoD2 原始诊断导出扫描", `exit=${redactExport.exit} hits=${JSON.stringify(rawExportHits)}`);
  }

  if (allZero(redactedExportHits)) {
    pass(`DoD2 脱敏后诊断导出 0 命中（sk-=${redactedExportHits["sk-"]}, eyJ=${redactedExportHits.eyJ}, PEM=${redactedExportHits.PEM}）`);
  } else {
    fail("DoD2 脱敏后诊断导出 0 命中", JSON.stringify(redactedExportHits));
  }

  let exportStructureKept = false;
  try {
    const parsed = JSON.parse(redactExport.stdout);
    exportStructureKept =
      parsed?.db?.size_bytes === 4096 &&
      parsed?.app?.name === "Aether" &&
      parsed?.security?.refs?.[0] === "keychain://aether/adapter-codex/api-key";
  } catch {
    exportStructureKept = false;
  }
  if (exportStructureKept) pass("DoD2 脱敏后诊断导出结构保留（引用与元数据可读）");
  else fail("DoD2 诊断导出结构", "JSON 结构或引用字段丢失");

  writeFileSync(
    path.join(outDir, "scan-report.json"),
    `${JSON.stringify(
      {
        rawLogHits: rawHits,
        redactedLogHits: redactedHits,
        rawExportHits: rawExportHits,
        redactedExportHits: redactedExportHits,
        samples: "运行期随机生成；原始样本不落盘",
      },
      null,
      2,
    )}\n`,
  );
}

// ---------------------------------------------------------------------------
// Step 3：真实 OS 凭据库自检（DoD1）
// ---------------------------------------------------------------------------
{
  const result = cargoRun(["run", "--quiet", "-p", "aether-security", "--example", "keychain_selfcheck"]);
  writeFileSync(
    path.join(outDir, "keychain-selfcheck.txt"),
    `${result.stdout}${result.stderr}`,
  );
  if (result.exit === 0 && result.stdout.includes("PASS")) {
    pass("DoD1 keychain 写→读→删自检（真实 OS 凭据库）通过");
  } else if (result.exit === 3 && process.platform !== "win32" && process.platform !== "darwin") {
    skip(
      "DoD1 keychain 自检",
      `本平台无可用凭据库（按 A3 设计降级）：${result.stderr.trim()}`,
    );
  } else {
    fail("DoD1 keychain 自检", `exit=${result.exit} ${result.stderr.trim()}`);
  }
}

// ---------------------------------------------------------------------------
// Step 4：降级路径演练（DoD3）
// ---------------------------------------------------------------------------
{
  const passphrase = `m1-07-${b64url(crypto.randomBytes(24))}`;
  const token = sampleApiKey();
  const dataDir = path.join(outDir, "drill");
  const result = cargoRun(
    [
      "run",
      "--quiet",
      "-p",
      "aether-security",
      "--example",
      "degraded_drill",
      "--",
      "--data-dir",
      dataDir,
    ],
    {
      env: {
        AETHER_M1_07_DRILL_PASSPHRASE: passphrase,
        AETHER_M1_07_DRILL_TOKEN: token,
      },
    },
  );
  const combined = `${result.stdout}\n${result.stderr}`;
  writeFileSync(path.join(outDir, "degraded-drill.txt"), combined);

  if (result.exit === 0 && result.stdout.includes("安全级别：降级")) {
    pass("DoD3 降级路径可启动并展示「安全级别：降级」");
  } else {
    fail("DoD3 降级启动", `exit=${result.exit}`);
  }

  if (!combined.includes(passphrase) && !combined.includes(token)) {
    pass("DoD3 演练口令与密钥不回显（stdout/stderr）");
  } else {
    fail("DoD3 演练输出泄漏口令或密钥");
  }

  const secretsFile = path.join(dataDir, "secrets.enc");
  if (!existsSync(secretsFile)) {
    fail("DoD3 secrets.enc 落盘", "文件不存在");
  } else {
    const bytes = readFileSync(secretsFile);
    const text = bytes.toString("latin1");
    if (!text.includes(token) && !text.includes(passphrase)) {
      pass("DoD3 secrets.enc 不含明文密钥/口令");
    } else {
      fail("DoD3 secrets.enc 明文泄漏");
    }
    let envelopeOk = false;
    try {
      const envelope = JSON.parse(bytes.toString("utf8"));
      envelopeOk =
        envelope.v === 1 &&
        envelope.kdf === "argon2id" &&
        typeof envelope.ciphertext === "string" &&
        envelope.ciphertext.length > 0;
    } catch {
      envelopeOk = false;
    }
    if (envelopeOk) pass("DoD3 降级文件为 A3 信封（argon2id + AEAD 密文）");
    else fail("DoD3 降级文件信封结构");
  }
}

// ---------------------------------------------------------------------------
// Step 5：仓库源文件密钥形态扫描（AGENTS §2.9：测试夹具不得含密钥）
// ---------------------------------------------------------------------------
{
  const crateRoot = path.join(repoRoot, "crates", "aether-security");
  const files = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith(".rs")) files.push(full);
    }
  };
  walk(crateRoot);
  const offenders = [];
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    for (const [name, pattern] of Object.entries(SOURCE_SCAN_PATTERNS)) {
      const hits = text.match(pattern) ?? [];
      if (hits.length > 0) {
        offenders.push(`${path.relative(repoRoot, file)} [${name}] x${hits.length}`);
      }
    }
  }
  writeFileSync(
    path.join(outDir, "source-secret-scan.txt"),
    offenders.length === 0
      ? `已扫描 ${files.length} 个 .rs 源文件：0 命中\n`
      : `${offenders.join("\n")}\n`,
  );
  if (offenders.length === 0) {
    pass(`AGENTS §2.9 源文件密钥形态扫描 0 命中（${files.length} 个 .rs）`);
  } else {
    fail("源文件密钥形态扫描", offenders.join("; "));
  }
}

console.log(`\n[scan-report] ${path.join(outDir, "scan-report.json")}`);
process.exit(summarize("m1-07 密钥存储与脱敏", checks));
