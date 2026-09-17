import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { StartupGate } from "./StartupGate";
import {
  exitApp,
  migrateDataDir,
  pickMigrationTarget,
  type StartupSnapshot,
} from "./startup";

vi.mock("./startup", async (importOriginal) => {
  const original = await importOriginal<typeof import("./startup")>();
  return {
    ...original,
    migrateDataDir: vi.fn(),
    pickMigrationTarget: vi.fn(),
    exitApp: vi.fn(),
  };
});

const migrateMock = vi.mocked(migrateDataDir);
const pickMock = vi.mocked(pickMigrationTarget);
const exitMock = vi.mocked(exitApp);

const blockedSnapshot: StartupSnapshot = {
  phase: "blocked_sync_dir",
  data_dir: "C:\\Users\\me\\OneDrive\\Aether",
  data_dir_source: "default",
  detection: {
    platform: "windows",
    candidate: "C:\\Users\\me\\OneDrive\\Aether",
    resolved: "C:\\Users\\me\\OneDrive\\Aether",
    verdict: "reject",
    checks: [],
    reasons: ["macOS：iCloud Drive 容器（Mobile Documents）：命中"],
    note: "macOS 同步盘检测精度受限（iCloud 采用路径前缀近似，未绑定 NSURLIsUbiquitousItemKey）；请确认目录不在 iCloud/CloudStorage 下。",
  },
};

const readySnapshot: StartupSnapshot = {
  phase: "ready",
  data_dir: "D:\\AetherData",
  data_dir_source: "migrated",
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("StartupGate", () => {
  it("渲染阻塞原因；退出按钮调用 app_exit", () => {
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={vi.fn()} />);
    expect(screen.getByTestId("startup-gate")).toBeTruthy();
    expect(screen.getByTestId("startup-reasons").textContent).toContain(
      "iCloud Drive 容器",
    );
    fireEvent.click(screen.getByTestId("startup-exit"));
    expect(exitMock).toHaveBeenCalledTimes(1);
  });

  it("降级精度提示（检测精度受限 + 手动确认指引）渲染在启动门", () => {
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={vi.fn()} />);
    const note = screen.getByTestId("startup-precision-note");
    expect(note.textContent).toContain("检测精度受限");
    expect(note.textContent).toContain(
      "请确认目录不在 iCloud/CloudStorage 下",
    );
  });

  it("「选择目录…」把系统选择器结果写入目标输入框", async () => {
    pickMock.mockResolvedValue("D:\\AetherData");
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={vi.fn()} />);
    fireEvent.click(screen.getByTestId("startup-pick"));
    await waitFor(() => {
      expect((screen.getByTestId("startup-target") as HTMLInputElement).value).toBe(
        "D:\\AetherData",
      );
    });
  });

  it("输入为空时迁移按钮禁用", () => {
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={vi.fn()} />);
    expect(
      (screen.getByTestId("startup-migrate") as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("点击迁移调用 startup_migrate 并回报新快照", async () => {
    migrateMock.mockResolvedValue(readySnapshot);
    const onMigrated = vi.fn();
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={onMigrated} />);
    fireEvent.change(screen.getByTestId("startup-target"), {
      target: { value: "D:\\AetherData" },
    });
    fireEvent.click(screen.getByTestId("startup-migrate"));
    await waitFor(() => {
      expect(onMigrated).toHaveBeenCalledWith(readySnapshot);
    });
    expect(migrateMock).toHaveBeenCalledWith("D:\\AetherData");
  });

  it("迁移失败展示结构化错误且不回调 onMigrated", async () => {
    migrateMock.mockRejectedValue({
      code: "path_rejected",
      message: "迁移目标必须为空目录",
    });
    const onMigrated = vi.fn();
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={onMigrated} />);
    fireEvent.change(screen.getByTestId("startup-target"), {
      target: { value: "D:\\Occupied" },
    });
    fireEvent.click(screen.getByTestId("startup-migrate"));
    expect(await screen.findByTestId("startup-error")).toBeTruthy();
    expect(screen.getByTestId("startup-error").textContent).toBe(
      "迁移目标必须为空目录",
    );
    expect(onMigrated).not.toHaveBeenCalled();
  });

  it("目录选择器失败时展示错误文本（非结构化错误走 String 兜底）", async () => {
    pickMock.mockRejectedValue("dialog unavailable");
    render(<StartupGate snapshot={blockedSnapshot} onMigrated={vi.fn()} />);
    fireEvent.click(screen.getByTestId("startup-pick"));
    expect(await screen.findByTestId("startup-error")).toBeTruthy();
    expect(screen.getByTestId("startup-error").textContent).toBe(
      "dialog unavailable",
    );
  });

  it("数据目录解析硬错误仅提供退出", () => {
    render(
      <StartupGate
        snapshot={{
          phase: "blocked_error",
          data_dir: "",
          data_dir_source: "default",
          message: "指针文件指向的数据目录不存在",
        }}
        onMigrated={vi.fn()}
      />,
    );
    expect(screen.getByTestId("startup-message").textContent).toContain(
      "指针文件指向的数据目录不存在",
    );
    expect(screen.queryByTestId("startup-migrate")).toBeNull();
    expect(screen.getByTestId("startup-exit")).toBeTruthy();
  });
});
