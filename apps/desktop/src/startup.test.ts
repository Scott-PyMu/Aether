import { describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";

import {
  describeIpcError,
  exitApp,
  fetchStartup,
  migrateDataDir,
  pickMigrationTarget,
  type StartupSnapshot,
} from "./startup";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const readySnapshot: StartupSnapshot = {
  phase: "ready",
  data_dir: "D:\\AetherData",
  data_dir_source: "migrated",
};

describe("startup IPC 契约", () => {
  it("fetchStartup 调用 startup_get", async () => {
    invokeMock.mockResolvedValue(readySnapshot);
    await expect(fetchStartup()).resolves.toEqual(readySnapshot);
    expect(invokeMock).toHaveBeenCalledWith("startup_get");
  });

  it("migrateDataDir 以 payload.target_dir 调用 startup_migrate", async () => {
    invokeMock.mockResolvedValue(readySnapshot);
    await expect(migrateDataDir("D:\\AetherData")).resolves.toEqual(
      readySnapshot,
    );
    expect(invokeMock).toHaveBeenCalledWith("startup_migrate", {
      payload: { target_dir: "D:\\AetherData" },
    });
  });

  it("pickMigrationTarget 返回选择结果，取消时为 null", async () => {
    invokeMock.mockResolvedValueOnce({ target_dir: "D:\\Picked" });
    await expect(pickMigrationTarget()).resolves.toBe("D:\\Picked");
    invokeMock.mockResolvedValueOnce({ target_dir: null });
    await expect(pickMigrationTarget()).resolves.toBeNull();
    expect(invokeMock).toHaveBeenCalledWith("startup_pick_target");
  });

  it("exitApp 调用 app_exit", async () => {
    invokeMock.mockResolvedValue(undefined);
    await expect(exitApp()).resolves.toBeUndefined();
    expect(invokeMock).toHaveBeenCalledWith("app_exit");
  });

  it("describeIpcError 优先 message，兜底 String", () => {
    expect(describeIpcError({ code: "path_rejected", message: "拒绝" })).toBe(
      "拒绝",
    );
    expect(describeIpcError({ code: "path_rejected" })).toBe(
      "[object Object]",
    );
    expect(describeIpcError("plain")).toBe("plain");
  });
});
