/**
 * M10-05/ADR-035 前端：附件采集/能力门/发送/失败重试（UI-UX `attachment` 锚点组）。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ArtifactsIpc } from "./artifacts";
import type { AttachmentIpc, AttachmentSummary } from "./attachments";
import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import type {
  MessagesPageResult,
  PermissionMode,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN_1 = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";
const ATTACHMENT = "01J8ZQ5R0N7W9Y8X6V4T2S0K1Z";
const SHA = "a".repeat(64);

function runtime(capabilities: string[]): RuntimeInfo {
  return {
    id: "claude-code",
    name: "Claude Code",
    kind: "claude",
    version: "0.1.0",
    protocol: "1.0",
    capabilities,
    enabled: true,
    status: "ready",
    status_reason: null,
  };
}

function sessionSummary(): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "claude-code",
    workspace_id: null,
    parent_session_id: null,
    title: "附件会话",
    status: "idle" as SessionStatus,
    model: "claude-opus",
    created_at: 1,
    updated_at: 1,
    closed_at: null,
  };
}

function emptyPage(): MessagesPageResult {
  return { session_id: SESSION, max_seq: null, events: [], messages: [], complete: true };
}

function fakeIpc(capabilities: string[]): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime(capabilities)]),
    searchSessions: vi.fn(async () => []),
    renameSession: vi.fn(async (sessionId: string, title: string) => ({
      session_id: sessionId,
      title,
      updated_at: 0,
    })),
    deleteSession: vi.fn(async (sessionId: string) => ({ session_id: sessionId, deleted: true })),
    listSessions: vi.fn(async () => [sessionSummary()]),
    createSession: vi.fn(async () => sessionSummary()),
    sendMessage: vi.fn(async (input) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async (sessionId) => ({ session_id: sessionId, interrupted_run: null })),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(async (sessionId) => ({
      session_id: sessionId,
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(),
    setPermissionMode: vi.fn(async (sessionId: string, mode: PermissionMode) => ({
      session_id: sessionId,
      permission_mode: mode,
      previous_permission_mode: "workspace_write",
    })),
  };
}

function fakeArtifacts(pickPath: string | null): ArtifactsIpc {
  return {
    pick: vi.fn(async () => pickPath),
    list: vi.fn(async () => []),
    add: vi.fn(),
    remove: vi.fn(async () => false),
  } as unknown as ArtifactsIpc;
}

function summary(): AttachmentSummary {
  return {
    attachment_id: ATTACHMENT,
    sha256: SHA,
    mime: "image/png",
    size_bytes: 4096,
    width: 64,
    height: 48,
    created_at: 1,
  };
}

function fakeAttachments(overrides: Partial<AttachmentIpc> = {}): AttachmentIpc {
  return {
    add: vi.fn(async () => summary()),
    list: vi.fn(async () => []),
    remove: vi.fn(async () => ({ removed: true })),
    ...overrides,
  };
}

const stores: EventStore[] = [];
afterEach(() => {
  cleanup();
  for (const store of stores.splice(0)) store.dispose();
  vi.restoreAllMocks();
});

async function open(
  ipc: SessionIpc,
  attachments: AttachmentIpc,
  pickPath: string | null,
): Promise<void> {
  const store = new EventStore();
  stores.push(store);
  render(
    <SessionWorkbench
      store={store}
      ipc={ipc}
      artifactsIpc={fakeArtifacts(pickPath)}
      attachmentIpc={attachments}
    />,
  );
  fireEvent.click(await screen.findByTestId("session-item"));
  await screen.findByTestId("composer-input");
}

describe("M10-05 附件输入区（ADR-035 / UI-UX attachment）", () => {
  it("选择图片 → 上传 ready（缩略图协议）→ 发送携带 attachments → 成功后清空", async () => {
    const sendMessage = vi.fn(async (input: Parameters<SessionIpc["sendMessage"]>[0]) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    }));
    const ipc = fakeIpc(["multimodal"]);
    ipc.sendMessage = sendMessage;
    const attachments = fakeAttachments();
    await open(ipc, attachments, "C:\\ws\\shot.png");

    const picker = screen.getByTestId("attachment-picker");
    expect(picker.getAttribute("data-enabled")).toBe("true");

    fireEvent.click(screen.getByTestId("composer-attach"));
    const item = await screen.findByTestId("attachment-item", {}, { timeout: 5000 });
    await waitFor(() => expect(item.getAttribute("data-state")).toBe("ready"));
    expect(item.getAttribute("data-attachment-id")).toBe(ATTACHMENT);
    expect(item.getAttribute("data-kind")).toBe("image");
    const preview = screen.getByTestId("attachment-preview");
    expect(preview.getAttribute("src")).toBe(`aether-attachment://thumb/${SHA}`);
    expect(attachments.add).toHaveBeenCalledWith(SESSION, "C:\\ws\\shot.png");

    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "看图" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1), { timeout: 5000 });
    const input = sendMessage.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(input.attachments).toEqual([ATTACHMENT]);
    await waitFor(() => {
      expect(screen.getByTestId("attachment-picker").getAttribute("data-count")).toBe("0");
    });
  });

  it("运行时未声明 multimodal → 入口置灰 + 不调用 attachment_add", async () => {
    const attachments = fakeAttachments();
    await open(fakeIpc([]), attachments, "C:\\ws\\shot.png");
    expect(screen.getByTestId("attachment-picker").getAttribute("data-enabled")).toBe("false");
    const button = screen.getByTestId("composer-attach") as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(attachments.add).not.toHaveBeenCalled();
  });

  it("上传失败 → data-state=failed + data-error-code；重试成功后 ready", async () => {
    const add = vi
      .fn()
      .mockRejectedValueOnce({ aether_code: "attachment_too_large", message: "too large" })
      .mockResolvedValueOnce(summary());
    const attachments = fakeAttachments({ add });
    await open(fakeIpc(["multimodal"]), attachments, "C:\\ws\\shot.png");

    fireEvent.click(screen.getByTestId("composer-attach"));
    const item = await screen.findByTestId("attachment-item", {}, { timeout: 5000 });
    await waitFor(() => expect(item.getAttribute("data-state")).toBe("failed"));
    expect(item.getAttribute("data-error-code")).toBe("attachment_too_large");

    fireEvent.click(item.querySelector('[data-action="retry"]') as HTMLButtonElement);
    await waitFor(() =>
      expect(screen.getByTestId("attachment-item").getAttribute("data-state")).toBe("ready"),
    );
    expect(add).toHaveBeenCalledTimes(2);
  });

  it("删除附件：调用 attachment_remove，发送不再携带", async () => {
    const sendMessage = vi.fn(async (input: Parameters<SessionIpc["sendMessage"]>[0]) => ({
      session_id: input.session_id,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: RUN_1,
      queued: false,
      duplicate: false,
    }));
    const ipc = fakeIpc(["multimodal"]);
    ipc.sendMessage = sendMessage;
    const attachments = fakeAttachments();
    await open(ipc, attachments, "C:\\ws\\shot.png");
    fireEvent.click(screen.getByTestId("composer-attach"));
    const item = await screen.findByTestId("attachment-item", {}, { timeout: 5000 });
    await waitFor(() => expect(item.getAttribute("data-state")).toBe("ready"));
    fireEvent.click(item.querySelector('[data-action="remove"]') as HTMLButtonElement);
    await waitFor(() => expect(attachments.remove).toHaveBeenCalledWith(ATTACHMENT));

    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "无附件" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1));
    expect(sendMessage.mock.calls[0]?.[0]).not.toHaveProperty("attachments");
  });
});
