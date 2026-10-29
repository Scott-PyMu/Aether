/**
 * 数据目录解析/日志脱敏/离线诊断单测（M9-07 DoD-4/DoD-6）。
 */
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { afterAll, describe, expect, it } from "vitest";

import {
  daemonLogPathFor,
  defaultDataDir,
  pointerFile,
  resolveDataDir,
  shellLogPathFor,
} from "./dataDir";
import {
  MANUAL_DATA_DIR_INSTRUCTIONS,
  SHELL_DIAGNOSTICS_SCHEMA,
  buildOfflineDiagnostics,
  defaultDiagnosticsFileName,
  writeOfflineDiagnostics,
} from "./offlineDiagnostics";
import { REDACTED, redactEnv, redactText, redactValue } from "./redact";
import { createShellLogger } from "./shellLog";

const root = mkdtempSync(path.join(tmpdir(), "aether-m9-07-data-"));
afterAll(() => {
  rmSync(root, { recursive: true, force: true });
});

describe("resolveDataDir", () => {
  it("AETHER_DATA_DIR 绝对路径优先", () => {
    const dir = path.join(root, "custom");
    const resolved = resolveDataDir({
      env: { AETHER_DATA_DIR: dir },
      platform: "win32",
      home: root,
    });
    expect(resolved).toEqual({ dataDir: path.normalize(dir), source: "env" });
  });

  it("AETHER_DATA_DIR 相对路径 → 缺省目录 + 错误说明", () => {
    const resolved = resolveDataDir({
      env: { AETHER_DATA_DIR: "relative/dir", APPDATA: path.join(root, "roaming") },
      platform: "win32",
      home: root,
    });
    expect(resolved.source).toBe("default");
    expect(resolved.error).toContain("必须是绝对路径");
    expect(resolved.dataDir).toBe(path.join(root, "roaming", "Aether"));
  });

  it("指针文件（v1 + 绝对路径）优先于缺省", () => {
    const pointer = path.join(root, "local", "data-location.json");
    const target = path.join(root, "moved");
    mkdirSync(path.dirname(pointer), { recursive: true });
    writeFileSync(pointer, JSON.stringify({ v: 1, data_dir: target }), "utf8");
    const resolved = resolveDataDir({
      env: { AETHER_DATA_LOCATION_FILE: pointer },
      platform: "win32",
      home: root,
    });
    expect(resolved.source).toBe("pointer");
    expect(resolved.dataDir).toBe(path.normalize(target));
  });

  it("指针损坏/版本不符 → 缺省 + 错误说明（不阻断 S-12 展示）", () => {
    const pointer = path.join(root, "broken", "data-location.json");
    mkdirSync(path.dirname(pointer), { recursive: true });
    writeFileSync(pointer, "{not-json", "utf8");
    const resolved = resolveDataDir({
      env: {
        AETHER_DATA_LOCATION_FILE: pointer,
        APPDATA: path.join(root, "roaming"),
      },
      platform: "win32",
      home: root,
    });
    expect(resolved.source).toBe("default");
    expect(resolved.error).toContain("读取数据目录指针失败");

    writeFileSync(pointer, JSON.stringify({ v: 99, data_dir: root }), "utf8");
    const versioned = resolveDataDir({
      env: { AETHER_DATA_LOCATION_FILE: pointer },
      platform: "win32",
      home: root,
    });
    expect(versioned.error).toContain("版本 99 不受支持");
  });

  it("平台缺省路径与日志路径", () => {
    expect(defaultDataDir("win32", root, { APPDATA: path.join(root, "roaming") })).toBe(
      path.join(root, "roaming", "Aether"),
    );
    expect(defaultDataDir("darwin", root)).toBe(
      path.join(root, "Library", "Application Support", "Aether"),
    );
    expect(defaultDataDir("linux", root, { XDG_DATA_HOME: path.join(root, "xdg") })).toBe(
      path.join(root, "xdg", "Aether"),
    );
    expect(pointerFile("win32", root, { LOCALAPPDATA: path.join(root, "local") })).toBe(
      path.join(root, "local", "Aether", "data-location.json"),
    );
    expect(shellLogPathFor(root)).toBe(path.join(root, "logs", "shell.log"));
    expect(daemonLogPathFor(root)).toBe(path.join(root, "logs", "aetherd.log"));
  });
});

describe("redact", () => {
  it("api_key/Bearer/sk- 前缀全部脱敏", () => {
    const text = [
      '{"api_key":"sk-abcdefghijklmnop","model":"x"}',
      "Authorization: Bearer abc.def.ghi",
      "token=plain-secret",
    ].join("\n");
    const redacted = redactText(text);
    expect(redacted).not.toContain("sk-abcdefghijklmnop");
    expect(redacted).not.toContain("abc.def.ghi");
    expect(redacted).not.toContain("plain-secret");
    expect(redacted).toContain(REDACTED);
  });

  it("结构化值/环境变量：密钥键剔除或打码", () => {
    expect(redactValue({ api_key: "x", nested: { password: "y", ok: "z" } })).toEqual({
      api_key: REDACTED,
      nested: { password: REDACTED, ok: "z" },
    });
    const env = redactEnv({
      PATH: "C:\\bin",
      AETHER_SECRETS_PASSPHRASE: "hunter2",
      OPENAI_API_KEY: "sk-123",
      AETHER_DATA_DIR: "C:\\Aether",
    });
    expect(env.PATH).toBe("C:\\bin");
    expect(env.AETHER_DATA_DIR).toBe("C:\\Aether");
    expect(env.AETHER_SECRETS_PASSPHRASE).toBeUndefined();
    expect(env.OPENAI_API_KEY).toBeUndefined();
  });
});

describe("offline diagnostics", () => {
  it("build 包含 S-12 原因/路径/手动移动说明，且脱敏", () => {
    const payload = buildOfflineDiagnostics({
      fatal: {
        reason: "db_open_failed",
        message: "数据库无法打开 token=secret-token",
        logPath: "C:\\Aether\\logs\\aetherd.log",
        dataDir: "C:\\Aether",
        attempts: 1,
      },
      versions: {
        app: "0.1.0",
        electron: "45.0.0",
        chrome: "140.0.0",
        node: "22.0.0",
        protocol: "1.0",
      },
      logs: { shell: "shell", daemon: "daemon", daemonStderr: "stderr" },
      env: { OPENAI_API_KEY: "sk-secretsecret" },
      now: 1_700_000_000_000,
      platform: "win32",
      arch: "x64",
    });
    expect(payload.schema).toBe(SHELL_DIAGNOSTICS_SCHEMA);
    expect(payload.kind).toBe("shell_fatal");
    expect(JSON.stringify(payload)).not.toContain("sk-secretsecret");
    expect(JSON.stringify(payload)).not.toContain("secret-token");
    expect(payload.manual_data_dir).toBe(MANUAL_DATA_DIR_INSTRUCTIONS);
    expect((payload.fatal as { reason: string }).reason).toBe("db_open_failed");
    expect((payload.paths as { data_dir: string }).data_dir).toBe("C:\\Aether");
  });

  it("写入绝对路径；相对路径/不存在目录报错", () => {
    const file = path.join(root, "diag.json");
    const written = writeOfflineDiagnostics(file, { ok: true });
    expect(written).toBe(path.normalize(file));
    expect(() => writeOfflineDiagnostics("relative.json", {})).toThrow("绝对路径");
    expect(() =>
      writeOfflineDiagnostics(path.join(root, "missing", "diag.json"), {}),
    ).toThrow("目录不存在");
  });

  it("默认文件名包含 schema 前缀且无冒号", () => {
    const name = defaultDiagnosticsFileName(0);
    expect(name.startsWith("aether-diagnostics-")).toBe(true);
    expect(name.includes(":")).toBe(false);
    expect(name.endsWith(".json")).toBe(true);
  });
});

describe("shell logger", () => {
  it("写入 JSONL、密钥脱敏；tail 可读", () => {
    const dir = path.join(root, "logger");
    const logger = createShellLogger({
      dataDir: dir,
      pid: 42,
      now: () => 1_700_000_000_000,
    });
    logger.log("info", "test.start", { api_key: "sk-abcdefghijkl", value: 1 });
    logger.log("error", "test.error", { message: "token=plain" });
    expect(logger.path).toBe(shellLogPathFor(dir));
    expect(logger.writable).toBe(true);
    const tail = logger.tail(10);
    expect(tail).toContain("test.start");
    expect(tail).toContain("test.error");
    expect(tail).not.toContain("sk-abcdefghijkl");
    expect(tail).not.toContain("plain");
  });

  it("超过上限轮转为 shell.log.1（只保留一份历史）", () => {
    const dir = path.join(root, "logger-rotate");
    const logger = createShellLogger({ dataDir: dir, maxBytes: 1 });
    logger.log("info", "first");
    logger.log("info", "second");
    expect(existsSync(`${shellLogPathFor(dir)}.1`)).toBe(true);
  });
});
