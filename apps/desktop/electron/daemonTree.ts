/**
 * daemon 进程树回收（M9-09 DoD-2/DoD-4；ADR-029 §3.6）。
 *
 * 口径：
 * - 正常路径先发 `shutdown` 并等待进程退出（预算 8.2s）；
 * - main 在 9.5s 仍未退出 → **整树回收**：
 *   - Windows：`taskkill /PID <pid> /T /F`（禁止裸 kill 单个 PID）；
 *   - Unix：daemon 以 `detached: true` 启动（setsid），向进程组 `-pid` 发
 *     `SIGTERM`，0.5s 后仍在则 `SIGKILL`；
 * - T11 总上限 10s（进程退出/无孤儿）。
 *
 * 本模块不依赖 Electron，便于单测；生产由 main 注入真实 pid。
 */
import { spawnSync } from "node:child_process";

/** daemon 正常关闭预算（ADR-029 §3.6：daemon 合计 8.2s）。 */
export const DAEMON_SHUTDOWN_BUDGET_MS = 8_200;
/** main 等待 daemon 退出的硬上限（ADR-029 §3.6：9.5s 到点强杀）。 */
export const MAIN_EXIT_HARD_BUDGET_MS = 9_500;
/** T11 总上限（ADR-029 §3.6 / 附录 D：≤10s）。 */
export const T11_BUDGET_MS = 10_000;
/** Unix 强杀前给进程组处理 SIGTERM 的余量（ADR-029 §3.6-3：0.5s）。 */
export const UNIX_TERM_GRACE_MS = 500;

export type TreeKillMechanism =
  | "taskkill_tree_force"
  | "sigterm_pgid"
  | "sigkill_pgid"
  | "none";

export interface TreeKillResult {
  killed: boolean;
  mechanism: TreeKillMechanism;
  detail?: string;
}

/** 注入点：`process.kill(pid, signal)`；测试可替换。 */
export type KillFunction = (pid: number, signal: NodeJS.Signals | 0) => boolean;

/** 注入点：`spawnSync` 的最小形状（只使用 status/error）。 */
export type TaskkillRunner = (
  command: string,
  args: readonly string[],
  options: {
    encoding: "utf8";
    timeout: number;
    windowsHide: boolean;
  },
) => { status: number | null; error?: Error; stderr?: string | null };

export interface TreeKillOptions {
  pid: number;
  platform?: NodeJS.Platform;
  /** 本次强杀可用预算（毫秒）；仅用于约束 taskkill/等待。 */
  timeoutMs?: number;
  kill?: KillFunction;
  taskkill?: TaskkillRunner;
  sleep?: (ms: number) => Promise<void>;
}

/** daemon spawn 选项：Unix 独立进程组（setsid），Windows 隐藏窗口。 */
export function daemonSpawnOptions(platform: NodeJS.Platform = process.platform): {
  detached: boolean;
  windowsHide: boolean;
} {
  return {
    detached: platform !== "win32",
    windowsHide: true,
  };
}

/** 进程是否存活（`EPERM` 视为存活：存在但无权限）。 */
export function pidAlive(pid: number, kill: KillFunction = process.kill): boolean {
  if (!Number.isInteger(pid) || pid <= 0) {
    return false;
  }
  try {
    kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * 整树回收 daemon 进程。
 *
 * 幂等：进程已不存在时返回 `{killed:true, mechanism:"none"}`（视为已回收）。
 */
export async function killDaemonTree(options: TreeKillOptions): Promise<TreeKillResult> {
  const platform = options.platform ?? process.platform;
  const kill = options.kill ?? process.kill;
  const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const timeoutMs = Math.max(250, options.timeoutMs ?? 2_000);

  if (!Number.isInteger(options.pid) || options.pid <= 0) {
    return { killed: true, mechanism: "none", detail: "pid 不可用（视为已回收）" };
  }
  if (!pidAlive(options.pid, kill)) {
    return { killed: true, mechanism: "none", detail: "进程已退出" };
  }

  if (platform === "win32") {
    const runner = options.taskkill ?? spawnSync;
    let result: { status: number | null; error?: Error; stderr?: string | Buffer | null };
    try {
      result = runner("taskkill", ["/PID", String(options.pid), "/T", "/F"], {
        encoding: "utf8",
        timeout: Math.max(1_000, Math.min(2_000, timeoutMs)),
        windowsHide: true,
      }) as { status: number | null; error?: Error; stderr?: string | Buffer | null };
    } catch (error) {
      return {
        killed: false,
        mechanism: "taskkill_tree_force",
        detail: `taskkill 调用失败：${String(error)}`,
      };
    }
    const alive = pidAlive(options.pid, kill);
    if (!alive) {
      return { killed: true, mechanism: "taskkill_tree_force" };
    }
    const stderr =
      typeof result.stderr === "string" ? result.stderr.trim() : "";
    return {
      killed: false,
      mechanism: "taskkill_tree_force",
      detail:
        result.error?.message ??
        (stderr.length > 0
          ? stderr
          : `taskkill 退出码 ${result.status ?? "null"}；进程仍存活`),
    };
  }

  // Unix：daemon 为会话/进程组首领（spawn detached），按 -pid 整组回收。
  const group = -options.pid;
  let mechanism: TreeKillMechanism = "sigterm_pgid";
  try {
    kill(group, "SIGTERM");
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code !== "ESRCH") {
      return { killed: false, mechanism: "sigterm_pgid", detail: String(error) };
    }
  }
  await sleep(Math.min(UNIX_TERM_GRACE_MS, timeoutMs));
  if (!pidAlive(options.pid, kill)) {
    return { killed: true, mechanism };
  }
  mechanism = "sigkill_pgid";
  try {
    kill(group, "SIGKILL");
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code !== "ESRCH") {
      return { killed: false, mechanism, detail: String(error) };
    }
  }
  // 给内核一点回收时间；最终仍存活则报告失败（由调用方记录，不掩盖）。
  await sleep(Math.min(100, timeoutMs));
  return pidAlive(options.pid, kill)
    ? { killed: false, mechanism, detail: "SIGKILL 后进程仍存活" }
    : { killed: true, mechanism };
}
