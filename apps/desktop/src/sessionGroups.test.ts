/**
 * M3-12：会话列表分组纯函数单测（UI-UX §2.2 固定组序 / 空组隐藏 / 组内倒序）。
 */
import { describe, expect, it } from "vitest";

import type { SessionStatus, SessionSummary } from "./session";
import {
  groupSessions,
  SESSION_GROUP_ORDER,
  sessionGroupOf,
} from "./sessionGroups";

function summary(
  id: string,
  status: SessionStatus,
  updatedAt: number,
): SessionSummary {
  return {
    id,
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: id,
    status,
    model: null,
    created_at: 1,
    updated_at: updatedAt,
    closed_at: null,
  };
}

describe("sessionGroups（M3-12）", () => {
  it("状态映射与固定组序一致（running/creating → 运行中）", () => {
    expect(sessionGroupOf("creating")).toBe("running");
    expect(sessionGroupOf("running")).toBe("running");
    expect(sessionGroupOf("waiting_permission")).toBe("waiting_permission");
    expect(sessionGroupOf("failed")).toBe("failed");
    for (const status of ["idle", "paused", "completed", "cancelled"] as SessionStatus[]) {
      expect(sessionGroupOf(status)).toBe("other");
    }
    expect(SESSION_GROUP_ORDER).toEqual([
      "running",
      "waiting_permission",
      "failed",
      "other",
    ]);
  });

  it("固定组序 + 空组隐藏 + 组内 updated_at 倒序", () => {
    const groups = groupSessions([
      summary("idle-new", "idle", 30),
      summary("failed-old", "failed", 10),
      summary("waiting", "waiting_permission", 20),
      summary("running-old", "running", 5),
      summary("idle-old", "idle", 1),
      summary("running-new", "running", 40),
      summary("completed", "completed", 60),
    ]);
    expect(groups.map((entry) => entry.group)).toEqual([
      "running",
      "waiting_permission",
      "failed",
      "other",
    ]);
    expect(groups[0]?.sessions.map((session) => session.id)).toEqual([
      "running-new",
      "running-old",
    ]);
    expect(groups[1]?.sessions.map((session) => session.id)).toEqual(["waiting"]);
    expect(groups[2]?.sessions.map((session) => session.id)).toEqual(["failed-old"]);
    expect(groups[3]?.sessions.map((session) => session.id)).toEqual([
      "completed",
      "idle-new",
      "idle-old",
    ]);
  });

  it("空列表/无对应状态组不产生分组（空组隐藏）", () => {
    expect(groupSessions([])).toEqual([]);
    const groups = groupSessions([summary("only-idle", "idle", 1)]);
    expect(groups.map((entry) => entry.group)).toEqual(["other"]);
  });
});
