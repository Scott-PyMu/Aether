/**
 * M5-05/M5-04 归档 JSONL 独立校验工具（ADR-021 附录 A-2）。
 *
 * 用法：node scripts/test/m5-05/verify-archive.mjs <archive.jsonl> [more...]
 *
 * 校验项：
 *   1) 首行为 manifest（`v=1`、`kind=manifest`、`created_at`/`source`/`session_id`/
 *      `rows`/`sha256` 齐备）；
 *   2) `rows` 与后续行数一致；`sha256` = 后续各行拼接内容（行尾 `\n` 归一）的 sha256；
 *   3) 逐行 JSON 对象、`v=1`、`kind ∈ {session,message,event,run}` 且字段在
 *      ADR-021 附录 A-2 白名单内（runs 行按实现登记表）；
 *   4) 脱敏扫描 0 命中（`sk-` / `eyJ` JWT / PEM 私钥块；D10）。
 *
 * 输出 JSON 摘要到 stdout；任一失败 exit=1。
 */
import { createHash } from "node:crypto";
import { createReadStream, existsSync } from "node:fs";
import process from "node:process";

const MANIFEST_FIELDS = ["v", "kind", "created_at", "source", "session_id", "rows", "sha256"];
const LINE_WHITELIST = {
  session: [
    "v", "kind", "id", "runtime_id", "workspace_id", "parent_session_id", "title", "status",
    "model", "thinking_depth", "system_prompt", "config", "token_usage", "created_at",
    "updated_at", "closed_at",
  ],
  message: ["v", "kind", "session_id", "id", "role", "content", "seq", "ts"],
  event: ["v", "kind", "id", "session_id", "run_id", "runtime_id", "seq", "ts", "type", "payload"],
  run: [
    "v", "kind", "id", "session_id", "status", "input_message_id", "thinking_depth", "error",
    "started_at", "finished_at",
  ],
};
const SECRET_PATTERNS = [
  /sk-[A-Za-z0-9_-]{12,}/,
  /eyJ[A-Za-z0-9_-]{6,}(?:\.[A-Za-z0-9_-]+)*/,
  /-----BEGIN[ A-Z0-9]*PRIVATE KEY[ A-Z0-9]*-----/,
];

async function verifyArchive(file) {
  const problems = [];
  const hasher = createHash("sha256");
  let rows = 0;
  let lineIndex = 0;
  let manifest = null;
  let kinds = {};

  let pending = Buffer.alloc(0);
  const handleLine = (line) => {
    const text = line.toString("utf8");
    if (lineIndex === 0) {
      try {
        manifest = JSON.parse(text);
      } catch (error) {
        problems.push(`manifest 不是合法 JSON：${error.message}`);
        return;
      }
      for (const field of MANIFEST_FIELDS) {
        if (!(field in manifest)) problems.push(`manifest 缺少字段：${field}`);
      }
      if (manifest.v !== 1) problems.push(`manifest.v 必须为 1：${manifest.v}`);
      if (manifest.kind !== "manifest") problems.push(`首行 kind 必须为 manifest：${manifest.kind}`);
      for (const pattern of SECRET_PATTERNS) {
        if (pattern.test(text)) problems.push("manifest 命中密钥模式（D10）");
      }
      return;
    }
    rows += 1;
    hasher.update(line);
    hasher.update("\n");
    let value;
    try {
      value = JSON.parse(text);
    } catch (error) {
      problems.push(`第 ${lineIndex + 1} 行不是合法 JSON：${error.message}`);
      return;
    }
    if (value.v !== 1) problems.push(`第 ${lineIndex + 1} 行 v 必须为 1：${value.v}`);
    const whitelist = LINE_WHITELIST[value.kind];
    if (!whitelist) {
      problems.push(`第 ${lineIndex + 1} 行未知 kind：${value.kind}`);
    } else {
      kinds[value.kind] = (kinds[value.kind] ?? 0) + 1;
      for (const key of Object.keys(value)) {
        if (!whitelist.includes(key)) {
          problems.push(`第 ${lineIndex + 1} 行字段不在白名单（kind=${value.kind}）：${key}`);
        }
      }
    }
    for (const pattern of SECRET_PATTERNS) {
      if (pattern.test(text)) {
        problems.push(`第 ${lineIndex + 1} 行命中密钥模式（D10）`);
        break;
      }
    }
  };

  const stream = createReadStream(file);
  for await (const chunk of stream) {
    pending = pending.length === 0 ? chunk : Buffer.concat([pending, chunk]);
    let index;
    while ((index = pending.indexOf(0x0a)) !== -1) {
      let line = pending.subarray(0, index);
      if (line.length > 0 && line[line.length - 1] === 0x0d) {
        line = line.subarray(0, line.length - 1);
      }
      handleLine(line);
      lineIndex += 1;
      pending = pending.subarray(index + 1);
    }
  }
  if (pending.length > 0) {
    handleLine(pending);
    lineIndex += 1;
  }

  if (!manifest) {
    problems.push("文件为空或缺少 manifest");
  } else {
    if (manifest.rows !== rows) {
      problems.push(`manifest.rows=${manifest.rows} 与实际行数 ${rows} 不一致`);
    }
    const digest = hasher.digest("hex");
    if (manifest.sha256 !== digest) {
      problems.push(`manifest.sha256=${manifest.sha256} 与实际 ${digest} 不一致`);
    }
  }
  return {
    file,
    ok: problems.length === 0,
    rows,
    kinds,
    source: manifest?.source ?? null,
    session_id: manifest?.session_id ?? null,
    problems,
  };
}

const files = process.argv.slice(2);
if (files.length === 0) {
  console.error("用法：node scripts/test/m5-05/verify-archive.mjs <archive.jsonl> [more...]");
  process.exit(2);
}

let failed = false;
for (const file of files) {
  if (!existsSync(file)) {
    console.log(JSON.stringify({ file, ok: false, problems: ["文件不存在"] }));
    failed = true;
    continue;
  }
  const result = await verifyArchive(file);
  console.log(JSON.stringify(result));
  if (!result.ok) failed = true;
}
process.exit(failed ? 1 : 0);
