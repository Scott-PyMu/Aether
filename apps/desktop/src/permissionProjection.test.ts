/**
 * 权限投影纯函数测试（M3-03）：清单 + 事件合并、超时判定、原文/规范化等价判定。
 */
import type { AetherEvent } from "@aether/protocol";
import { describe, expect, it } from "vitest";

import type { PendingPermission } from "./permission";
import { projectPermissions, targetsEquivalent } from "./permissionProjection";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const REQUEST = "01J8ZQ5R0N7W9Y8X6V4T2S0K1P";
const TICKET = "01J8ZQ5R0N7W9Y8X6V4T2S0K1T";

function event(type: string, payload: unknown, ts: number): AetherEvent {
  return {
    v: 1,
    id: `01J${String(ts).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: null,
    runtime_id: "mock",
    seq: 1,
    ts,
    type,
    payload,
  };
}

function pending(overrides: Partial<PendingPermission> = {}): PendingPermission {
  return {
    id: TICKET,
    request_id: REQUEST,
    session_id: SESSION,
    resource: "fs.write",
    action: "write",
    target: "D:\\ws\\a.txt",
    canonical_target: "D:\\ws\\a.txt",
    requested_at: 1_700_000_000_000,
    timeout_ms: 300_000,
    ...overrides,
  };
}

describe("projectPermissions", () => {
  it("清单提供 canonical 对照；事件增量合并不覆盖清单字段", () => {
    const items = projectPermissions(
      [
        event(
          "permission.requested",
          { request_id: REQUEST, resource: "fs.write", action: "write", target: "D:\\ws\\a.txt" },
          1_700_000_000_100,
        ),
      ],
      [pending()],
    );
    expect(items).toHaveLength(1);
    expect(items[0]?.status).toBe("pending");
    expect(items[0]?.canonicalTarget).toBe("D:\\ws\\a.txt");
    expect(items[0]?.requestedAt).toBe(1_700_000_000_000);
  });

  it("仅事件（无清单）也生成待审批项；resolved 事件收口决议与作用域", () => {
    const items = projectPermissions(
      [
        event(
          "permission.requested",
          { request_id: REQUEST, resource: "fs.write", action: "write", target: "t" },
          1_000,
        ),
        event(
          "permission.resolved",
          { request_id: REQUEST, decision: "allow", scope: "session" },
          2_000,
        ),
      ],
      [],
    );
    expect(items).toHaveLength(1);
    expect(items[0]?.status).toBe("resolved");
    expect(items[0]?.decision).toBe("allow");
    expect(items[0]?.scope).toBe("session");
    expect(items[0]?.canonicalTarget).toBeNull();
  });

  it("决议不早于 requested_at + timeout 的 deny 判为 timeout；更早的 deny 保持 resolved", () => {
    const timedOut = projectPermissions(
      [
        event(
          "permission.resolved",
          { request_id: REQUEST, decision: "deny", scope: null },
          1_700_000_300_000,
        ),
      ],
      [pending()],
    );
    expect(timedOut[0]?.status).toBe("timeout");

    const earlyDeny = projectPermissions(
      [
        event(
          "permission.resolved",
          { request_id: REQUEST, decision: "deny", scope: null },
          1_700_000_299_999,
        ),
      ],
      [pending()],
    );
    expect(earlyDeny[0]?.status).toBe("resolved");
  });
});

describe("targetsEquivalent（data-equal 语义）", () => {
  it("完全相同视为一致", () => {
    expect(targetsEquivalent("D:\\ws\\a.txt", "D:\\ws\\a.txt")).toBe(true);
  });

  it("Windows \\\\?\\ 扩展前缀剥离后大小写不敏感比较", () => {
    expect(targetsEquivalent("C:\\WS\\A.TXT", "\\\\?\\c:\\ws\\a.txt")).toBe(true);
  });

  it("真实差异判为不一致；缺失任一侧为未知", () => {
    expect(targetsEquivalent("D:\\ws\\link.txt", "D:\\ws\\real.txt")).toBe(false);
    expect(targetsEquivalent(null, "D:\\ws\\a.txt")).toBeNull();
    expect(targetsEquivalent("D:\\ws\\a.txt", null)).toBeNull();
  });
});
