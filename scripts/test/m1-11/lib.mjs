/**
 * M1-11 A1 spike 公共工具（Node 内置模块，无第三方依赖）。
 *
 * 约束：
 * - 不引入 Aether 核心代码；本目录仅用于 spike 证据采集。
 * - 任何密钥只经环境变量/进程内临时文件传递，不得写入仓库文件。
 */
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

export const nowMs = () => Number(process.hrtime.bigint() / 1_000_000n);

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function ensureDir(dir) {
  mkdirSync(dir, { recursive: true });
  return dir;
}

export function writeJson(file, data) {
  ensureDir(join(file, '..'));
  writeFileSync(file, `${JSON.stringify(data, null, 2)}\n`, 'utf8');
}

export function makeTmpDir(prefix) {
  const dir = join(tmpdir(), `${prefix}-${randomUUID()}`);
  ensureDir(dir);
  return dir;
}

const KNOWN_NPM_CLIS = {
  codex: '@openai/codex',
  claude: '@anthropic-ai/claude-code',
  dsh: '@deepseek-ai/dsh',
};

function npmGlobalRoot() {
  if (process.env.M1_11_NPM_GLOBAL_ROOT) return process.env.M1_11_NPM_GLOBAL_ROOT;
  if (process.platform === 'win32' && process.env.APPDATA) {
    return join(process.env.APPDATA, 'npm', 'node_modules');
  }
  const result = spawnSync('npm', ['root', '-g'], { encoding: 'utf8' });
  return result.status === 0 ? String(result.stdout).trim() : null;
}

/**
 * 解析 CLI。优先直连 npm 全局安装的包入口（.js 用当前 node 拉起 / .exe 直接执行），
 * 避免 .cmd shim 带来的引号二次解析；找不到时回退 where/which。
 */
export function resolveCli(name) {
  const packageName = KNOWN_NPM_CLIS[name];
  const root = npmGlobalRoot();
  if (packageName && root) {
    const packageDir = join(root, ...packageName.split('/'));
    const packageJson = join(packageDir, 'package.json');
    if (existsSync(packageJson)) {
      try {
        const binField = JSON.parse(readFileSync(packageJson, 'utf8')).bin;
        const binRelative = typeof binField === 'string' ? binField : binField?.[name];
        if (binRelative) {
          const binPath = join(packageDir, binRelative);
          if (existsSync(binPath)) {
            if (/\.(m?js)$/i.test(binPath)) {
              return { path: process.execPath, args: [binPath], viaShell: false, resolvedPath: binPath };
            }
            return { path: binPath, args: [], viaShell: false, resolvedPath: binPath };
          }
        }
      } catch {
        /* package.json 解析失败时回退 where */
      }
    }
  }

  const finder = process.platform === 'win32' ? 'where.exe' : 'which';
  const result = spawnSync(finder, [name], { encoding: 'utf8' });
  if (result.status !== 0) {
    throw new Error(`CLI 不可用：${name}（${finder} 退出码 ${result.status}）`);
  }
  const candidates = String(result.stdout)
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  const exe = candidates.find((p) => /\.exe$/i.test(p));
  if (exe) return { path: exe, args: [], viaShell: false, resolvedPath: exe };
  const cmd = candidates.find((p) => /\.(cmd|bat)$/i.test(p));
  if (cmd) return { path: cmd, args: [], viaShell: true, resolvedPath: cmd };
  return { path: candidates[0], args: [], viaShell: false, resolvedPath: candidates[0] };
}

function assertArgSafe(arg) {
  if (/["\r\n]/.test(arg)) {
    throw new Error(`拒绝传递含引号/换行的参数（cmd 转义歧义）：${JSON.stringify(arg)}`);
  }
}

/**
 * 通过 .cmd/.bat shim 启动时，使用 cmd.exe /d /s /c 包裹整条命令，
 * 避免 Node 在 Windows 上的 shell 二次解析（参考 hermes-studio 的 Windows shim 处理思路）。
 */
function buildCmdLine(exePath, args) {
  const quotedExe = `"${exePath}"`;
  const parts = args.map((arg) => {
    assertArgSafe(arg);
    return /[ \t]/.test(arg) ? `"${arg}"` : arg;
  });
  return `"${[quotedExe, ...parts].join(' ')}"`;
}

export function spawnCli(cli, args, options = {}) {
  const fullArgs = [...(cli.args || []), ...args];
  const child = cli.viaShell
    ? spawn(process.env.ComSpec || 'cmd.exe', ['/d', '/s', '/c', buildCmdLine(cli.path, fullArgs)], {
        ...options,
        windowsVerbatimArguments: true,
      })
    : spawn(cli.path, fullArgs, options);
  child.spawnedAt = nowMs();
  return child;
}

/** 按行解析 stdout/stderr；onLine(line, receivedAtMs)。 */
export function readLines(stream, onLine) {
  let buffer = '';
  stream.setEncoding('utf8');
  stream.on('data', (chunk) => {
    buffer += chunk;
    let index;
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index).replace(/\r$/, '');
      buffer = buffer.slice(index + 1);
      if (line.trim()) onLine(line, nowMs());
    }
  });
  stream.on('end', () => {
    const rest = buffer.trim();
    if (rest) onLine(rest, nowMs());
  });
}

/** 等待进程关闭（含超时）。进程已关闭时立即返回。 */
export function waitClose(child, timeoutMs) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (outcome) => {
      if (!settled) {
        settled = true;
        resolve(outcome);
      }
    };
    if (child.exitCode !== null || child.signalCode !== null) {
      finish({ kind: 'close', code: child.exitCode, signal: child.signalCode });
      return;
    }
    child.once('error', (error) => finish({ kind: 'spawn_error', error: error.message }));
    child.once('close', (code, signal) => finish({ kind: 'close', code, signal }));
    if (timeoutMs) {
      setTimeout(() => finish({ kind: 'timeout' }), timeoutMs).unref();
    }
  });
}

/**
 * 中断请求（DoD 2③）：Windows 走 taskkill /T /F 回收整树；POSIX 对进程组发 SIGINT。
 * 返回 { elapsedMs, code, signal }，elapsedMs 为「发起中断 → 进程关闭」耗时。
 */
export async function interruptTree(child) {
  const start = nowMs();
  const closed = waitClose(child, 30000);
  if (process.platform === 'win32') {
    spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  } else {
    try {
      process.kill(-child.pid, 'SIGINT');
    } catch {
      child.kill('SIGINT');
    }
  }
  const outcome = await closed;
  return { elapsedMs: nowMs() - start, ...outcome };
}

export function normalizeText(text) {
  if (typeof text !== 'string') return '';
  return text.replace(/\r\n/g, '\n').replace(/[ \t]+$/gm, '').trim();
}

export function stats(values) {
  const sorted = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (sorted.length === 0) return { count: 0 };
  const pick = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * (sorted.length - 1)))];
  const sum = sorted.reduce((acc, v) => acc + v, 0);
  return {
    count: sorted.length,
    min: sorted[0],
    p50: pick(0.5),
    p95: pick(0.95),
    max: sorted[sorted.length - 1],
    mean: Math.round((sum / sorted.length) * 100) / 100,
  };
}

/** 日志脱敏：任何写出到仓库/证据文件的文本都要过这里。 */
export function redact(text, secrets = []) {
  let output = String(text ?? '');
  for (const secret of secrets) {
    if (secret && secret.length >= 8) {
      output = output.split(secret).join('<redacted>');
    }
  }
  return output;
}

export function collectSecrets(...names) {
  const values = [];
  for (const name of names) {
    const value = process.env[name];
    if (value && value.length >= 8) values.push(value);
  }
  return values;
}

export function parseArgs(argv) {
  const args = { _: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    if (token.startsWith('--')) {
      const key = token.slice(2);
      const next = argv[i + 1];
      if (next === undefined || next.startsWith('--')) {
        args[key] = true;
      } else {
        args[key] = next;
        i += 1;
      }
    } else {
      args._.push(token);
    }
  }
  return args;
}

export function envFirst(names, fallback = '') {
  for (const name of names) {
    if (process.env[name]) return process.env[name];
  }
  return fallback;
}

export function removeDir(dir) {
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    /* 清理失败不影响结论 */
  }
}
