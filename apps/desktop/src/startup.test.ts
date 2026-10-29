import { describe, expect, it, vi } from "vitest";
import { client } from "@aether/protocol";

import {
  describeIpcError,
  exitApp,
  fetchStartup,
  ipcErrorCode,
  migrateDataDir,
  pickMigrationTarget,
  type StartupSnapshot,
} from "./startup";

vi.mock("@aether/protocol", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@aether/protocol")>();
  return {
    ...actual,
    client: {
      ...actual.client,
      startupGet: vi.fn(),
      startupMigrate: vi.fn(),
      startupPickTarget: vi.fn(),
      appExit: vi.fn(),
    },
  };
});

const startupGetMock = vi.mocked(client.startupGet);
const startupMigrateMock = vi.mocked(client.startupMigrate);
const startupPickTargetMock = vi.mocked(client.startupPickTarget);
const appExitMock = vi.mocked(client.appExit);

const readySnapshot: StartupSnapshot = {
  phase: "ready",
  data_dir: "D:\\AetherData",
  data_dir_source: "migrated",
};

describe("startup typed client 契约", () => {
  it("fetchStartup 调用 startup_get", async () => {
    startupGetMock.mockResolvedValue(readySnapshot);
    await expect(fetchStartup()).resolves.toEqual(readySnapshot);
    expect(startupGetMock).toHaveBeenCalledWith();
  });

  it("migrateDataDir 以 target_dir 调用 startup_migrate", async () => {
    startupMigrateMock.mockResolvedValue(readySnapshot);
    await expect(migrateDataDir("D:\\AetherData")).resolves.toEqual(
      readySnapshot,
    );
    expect(startupMigrateMock).toHaveBeenCalledWith({
      target_dir: "D:\\AetherData",
    });
  });

  it("pickMigrationTarget 返回选择结果，取消时为 null", async () => {
    startupPickTargetMock.mockResolvedValueOnce({ target_dir: "D:\\Picked" });
    await expect(pickMigrationTarget()).resolves.toBe("D:\\Picked");
    startupPickTargetMock.mockResolvedValueOnce({ target_dir: null });
    await expect(pickMigrationTarget()).resolves.toBeNull();
    expect(startupPickTargetMock).toHaveBeenCalledWith();
  });

  it("exitApp 调用 app_exit 并携带 confirm:true（ADR-006 v0.3）", async () => {
    appExitMock.mockResolvedValue({ exiting: true });
    await expect(exitApp()).resolves.toBeUndefined();
    expect(appExitMock).toHaveBeenCalledWith({ confirm: true });
  });

  it("describeIpcError 优先 message，兜底结构化 code / String", () => {
    expect(describeIpcError({ code: "path_rejected", message: "拒绝" })).toBe(
      "拒绝",
    );
    expect(describeIpcError({ code: "path_rejected" })).toBe("path_rejected");
    expect(describeIpcError("plain")).toBe("plain");
  });

  it("ipcErrorCode 兼容 AetherError 与旧 code 对象（business_code 优先）", () => {
    expect(
      ipcErrorCode({
        aether_code: "invalid_value",
        business_code: "session_busy",
        message: "忙",
      }),
    ).toBe("session_busy");
    expect(ipcErrorCode({ code: "core_not_ready" })).toBe("core_not_ready");
    expect(ipcErrorCode(new Error("plain"))).toBeNull();
  });
});
