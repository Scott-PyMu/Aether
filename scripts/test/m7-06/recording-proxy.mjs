#!/usr/bin/env node
/**
 * M7-06/ADR-024 DoD3 记录型代理：把运行时发往「供应商 base_url（本代理）」的请求
 * 记录为 JSONL（**不记录任何密钥本体**），并转发到真实上游（认证统一重写为
 * `Authorization: Bearer <上游 token>`，去掉 `x-api-key`）。
 *
 * 证据口径：记录 `key_match`（运行时实际携带的凭据 == 核心注入的测试密钥）与
 * `model`（请求体中的模型），用于断言「配置供应商 → run 真实使用该 base URL/模型」。
 *
 * 用法：
 *   node recording-proxy.mjs --listen 0 --log <jsonl> --runtime <id> \
 *     --upstream <url> --token <upstream-key> --expect-key <injected-test-key>
 * 就绪输出（stdout 单行 JSON）：{"ready":true,"port":<port>}
 */
import http from "node:http";
import https from "node:https";
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import process from "node:process";

function parseArgs(argv) {
  const out = {};
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith("--")) continue;
    const key = flag.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) out[key] = true;
    else {
      out[key] = value;
      index += 1;
    }
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
for (const required of ["listen", "log", "runtime", "upstream", "token"]) {
  if (args[required] === undefined) {
    console.error(`recording-proxy：缺少 --${required}`);
    process.exit(2);
  }
}
const upstream = new URL(args.upstream);
const upstreamBasePath = upstream.pathname.replace(/\/+$/, "");
const expectKey = typeof args["expect-key"] === "string" ? args["expect-key"] : null;
const listenPort = Number(args.listen);

mkdirSync(dirname(args.log), { recursive: true });

function record(entry) {
  try {
    appendFileSync(args.log, `${JSON.stringify(entry)}\n`);
  } catch {
    /* 日志失败不影响转发 */
  }
}

const server = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    let model = null;
    try {
      const parsed = JSON.parse(body.toString("utf8"));
      if (typeof parsed?.model === "string") model = parsed.model;
    } catch {
      /* 非 JSON 体（如 /models GET） */
    }
    const authorization = String(req.headers["authorization"] ?? "");
    const xApiKey = String(req.headers["x-api-key"] ?? "");
    const provided = authorization.toLowerCase().startsWith("bearer ")
      ? authorization.slice(7)
      : xApiKey;
    record({
      runtime: args.runtime,
      method: req.method ?? "",
      path: req.url ?? "",
      model,
      key_present: provided.length > 0,
      key_match: expectKey === null ? null : provided === expectKey,
      ts: Date.now(),
    });

    const headers = { ...req.headers };
    delete headers["x-api-key"];
    headers["authorization"] = `Bearer ${args.token}`;
    headers["host"] = upstream.host;
    if (body.length > 0) headers["content-length"] = String(body.length);
    const transport = upstream.protocol === "https:" ? https : http;
    const forwarded = transport.request(
      {
        protocol: upstream.protocol,
        hostname: upstream.hostname,
        port: upstream.port,
        method: req.method,
        path: `${upstreamBasePath}${req.url ?? "/"}`,
        headers,
      },
      (upstreamRes) => {
        // 追加转发结果（上游状态码；诊断/证据用，不含任何密钥）。
        record({
          runtime: args.runtime,
          method: req.method ?? "",
          path: req.url ?? "",
          upstream_status: upstreamRes.statusCode ?? null,
          ts: Date.now(),
        });
        res.writeHead(upstreamRes.statusCode ?? 502, upstreamRes.headers);
        upstreamRes.pipe(res);
      },
    );
    forwarded.on("error", (error) => {
      record({ runtime: args.runtime, error: String(error), ts: Date.now() });
      if (!res.headersSent) {
        res.writeHead(502, { "content-type": "application/json" });
      }
      res.end(JSON.stringify({ error: String(error) }));
    });
    forwarded.setTimeout(150_000, () => forwarded.destroy(new Error("上游超时")));
    forwarded.end(body);
  });
});

server.listen(listenPort, "127.0.0.1", () => {
  const address = server.address();
  process.stdout.write(`${JSON.stringify({ ready: true, port: address.port })}\n`);
});

process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
