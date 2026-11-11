/**
 * M10-05/ADR-035：附件工具与 IPC 包装单测（轻量前置校验/展示/协议 URL）。
 */
import { describe, expect, it, vi } from "vitest";

const client = vi.hoisted(() => ({
  attachmentAdd: vi.fn(),
  attachmentList: vi.fn(),
  attachmentRemove: vi.fn(),
}));

vi.mock("@aether/protocol", () => ({ client }));

import {
  attachmentDisplayName,
  attachmentIpc,
  attachmentThumbUrl,
  formatAttachmentBytes,
  runtimeSupportsMultimodal,
  validateAttachmentCandidate,
  ATTACHMENT_MAX_BYTES,
} from "./attachments";

describe("attachments.ts（M10-05）", () => {
  it("attachmentIpc 映射 typed client 参数/返回", async () => {
    client.attachmentAdd.mockResolvedValueOnce({
      attachment_id: "a1",
      sha256: "a".repeat(64),
      mime: "image/png",
      size_bytes: 1,
      created_at: 1,
    });
    const added = await attachmentIpc.add("s1", "C:\\tmp\\a.png");
    expect(client.attachmentAdd).toHaveBeenCalledWith({
      session_id: "s1",
      source_path: "C:\\tmp\\a.png",
      kind: "image",
    });
    expect(added.attachment_id).toBe("a1");

    client.attachmentList.mockResolvedValueOnce({ items: [] });
    expect(await attachmentIpc.list("s1")).toEqual([]);
    client.attachmentList.mockResolvedValueOnce({});
    expect(await attachmentIpc.list("s1")).toEqual([]);

    client.attachmentRemove.mockResolvedValueOnce({ removed: true });
    expect(await attachmentIpc.remove("a1")).toEqual({ removed: true });
    expect(client.attachmentRemove).toHaveBeenCalledWith({ attachment_id: "a1" });
  });

  it("缩略图 URL 与运行时能力判定", () => {
    expect(attachmentThumbUrl("abc")).toBe("aether-attachment://thumb/abc");
    expect(runtimeSupportsMultimodal(["multimodal"])).toBe(true);
    expect(runtimeSupportsMultimodal(["thinking_depth"])).toBe(false);
    expect(runtimeSupportsMultimodal(undefined)).toBe(false);
  });

  it("前置校验：大小/MIME/扩展名", () => {
    expect(
      validateAttachmentCandidate({
        size: ATTACHMENT_MAX_BYTES + 1,
        type: "image/png",
        name: "a.png",
      }),
    ).toBe("attachment_too_large");
    expect(
      validateAttachmentCandidate({ size: 1, type: "image/svg+xml", name: "a.svg" }),
    ).toBe("attachment_type_unsupported");
    expect(
      validateAttachmentCandidate({ size: 1, type: "image/png", name: "a.svg" }),
    ).toBe("attachment_type_unsupported");
    expect(validateAttachmentCandidate({ size: 1, type: "", name: "a.PNG" })).toBeNull();
    expect(validateAttachmentCandidate({ size: 1, type: "image/png", name: "noext" })).toBe(
      "attachment_type_unsupported",
    );
    expect(validateAttachmentCandidate({ size: 1, type: "image/png" })).toBeNull();
  });

  it("展示名清洗与大小格式", () => {
    expect(attachmentDisplayName("C:\\tmp\\pic.png")).toBe("pic.png");
    expect(attachmentDisplayName("/tmp/\u202Epic\n.png")).toBe("pic.png");
    expect(attachmentDisplayName("   ")).toBe("attachment");
    expect(formatAttachmentBytes(0)).toBe("0 B");
    expect(formatAttachmentBytes(1024)).toBe("1 KiB");
    expect(formatAttachmentBytes(1536 * 1024)).toBe("1.5 MiB");
  });
});
