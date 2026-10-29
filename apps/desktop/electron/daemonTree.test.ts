/**
 * M9-09 进程树回收单测（daemon ≤8.2s / main 9.5s 强杀 / T11 ≤10s）。
 */
import { describe, expect, it, vi } from "vitest";

import {
  DAEMON_SHUTDOWN_BUDGET_MS,
  MAIN_EXIT_HARD_BUDGET_MS,
  T11_BUDGET_MS,
  UNIX_TERM_GRACE_MS,
  daemonSpawnOptions,
  killDaemonTree,
  pidAlive,
} from "./daemonTree";

describe("冻结预算常量（ADR-029 §3.6）", () => {
  it("daemon 8.2s / main 9.5s / T11 10s", () => {
    expect(DAEMON_SHUTDOWN_BUDGET_MS).toBe(8_200);
    expect(MAIN_EXIT_HARD_BUDGET_MS).toBe(9_500);
    expect(T11_BUDGET_MS).toBe(10_000);
    expect(MAIN_EXIT_HARD_BUDGET_MS).toBeLessThan(T11_BUDGET_MS);
    expect(UNIX_TERM_GRACE_MS).toBe(500);
  });

  it("Unix 使用独立进程组；Windows 不 detached（taskkill /T 负责树）", () => {
    expect(daemonSpawnOptions("linux")).toEqual({ detached: true, windowsHide: true });
    expect(daemonSpawnOptions("darwin")).toEqual({ detached: true, windowsHide: true });
    expect(daemonSpawnOptions("win32")).toEqual({ detached: false, windowsHide: true });
  });
});

describe("killDaemonTree", () => {
  it("进程不存在时视为已回收（幂等）", async () => {
    const kill = vi.fn(() => {
      const error = new Error("gone") as NodeJS.ErrnoException;
      error.code = "ESRCH";
      throw error;
    });
    const result = await killDaemonTree({ pid: 4_242, platform: "linux", kill });
    expect(result).toMatchObject({ killed: true, mechanism: "none" });
  });

  it("Windows 使用 taskkill /PID /T /F（禁止裸 kill 单 PID）", async () => {
    let alive = true;
    const kill = vi.fn(() => {
      if (!alive) {
        const error = new Error("gone") as NodeJS.ErrnoException;
        error.code = "ESRCH";
        throw error;
      }
      return true;
    });
    const taskkill = vi.fn(() => {
      alive = false;
      return { status: 0, stderr: "" };
    });
    const result = await killDaemonTree({
      pid: 123,
      platform: "win32",
      kill,
      taskkill,
    });
    expect(taskkill).toHaveBeenCalledWith(
      "taskkill",
      ["/PID", "123", "/T", "/F"],
      expect.objectContaining({ timeout: 2_000, windowsHide: true }),
    );
    expect(result).toMatchObject({ killed: true, mechanism: "taskkill_tree_force" });
  });

  it("Unix 先 SIGTERM 进程组，0.5s 后仍存活再 SIGKILL 进程组", async () => {
    let alive = true;
    const signals: Array<{ pid: number; signal: NodeJS.Signals | 0 }> = [];
    const kill = vi.fn((pid: number, signal: NodeJS.Signals | 0) => {
      signals.push({ pid, signal });
      if (signal === 0) {
        if (!alive) {
          const error = new Error("gone") as NodeJS.ErrnoException;
          error.code = "ESRCH";
          throw error;
        }
        return true;
      }
      if (signal === "SIGKILL") {
        alive = false;
      }
      return true;
    });
    const sleeps: number[] = [];
    const result = await killDaemonTree({
      pid: 321,
      platform: "linux",
      kill,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });
    expect(signals.filter((item) => item.signal !== 0)).toEqual([
      { pid: -321, signal: "SIGTERM" },
      { pid: -321, signal: "SIGKILL" },
    ]);
    expect(sleeps[0]).toBe(500);
    expect(result).toMatchObject({ killed: true, mechanism: "sigkill_pgid" });
  });

  it("pidAlive 把 EPERM 视为存活", () => {
    const kill = vi.fn(() => {
      const error = new Error("denied") as NodeJS.ErrnoException;
      error.code = "EPERM";
      throw error;
    });
    expect(pidAlive(42, kill)).toBe(true);
    expect(pidAlive(0, kill)).toBe(false);
  });
});
