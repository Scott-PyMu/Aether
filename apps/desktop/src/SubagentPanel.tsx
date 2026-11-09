/**
 * M10-02/ADR-032 UI-03：子会话树 + 结果卡（锚点 `subagent-tree` / `subagent-node` /
 * `subagent-result` / `subagent-result-action`；不新增 testid）。
 *
 * 实时更新：父会话事件流出现 `subagent.*` 时重拉 `subagent_list`；`completed` 后
 * 以列表 `injection_state` 为权威（不依赖内存状态）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { EventStore } from "./eventStore";
import {
  SUBAGENT_INJECTION_LABELS,
  SUBAGENT_VERDICT_LABELS,
  subagentIpc,
  type SubagentIpc,
  type SubagentNode,
} from "./subagents";
import { useSessionEvents } from "./useSessionEvents";

export interface SubagentPanelProps {
  parentSessionId: string;
  store: EventStore;
  ipc?: SubagentIpc;
  onOpenChild?: (childSessionId: string) => void;
}

const ACTIVE_STATUSES = new Set(["creating", "idle", "running", "waiting_permission"]);

export function SubagentPanel({
  parentSessionId,
  store,
  ipc = subagentIpc,
  onOpenChild,
}: SubagentPanelProps) {
  const [nodes, setNodes] = useState<SubagentNode[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busyChild, setBusyChild] = useState<string | null>(null);
  const [details, setDetails] = useState<Record<string, string>>({});
  const [spawnBusy, setSpawnBusy] = useState(false);

  const eventsState = useSessionEvents(store, parentSessionId);
  const subagentEventCount = useMemo(
    () =>
      eventsState.events.filter((event) => event.type.startsWith("subagent."))
        .length,
    [eventsState],
  );

  const reload = useCallback(async () => {
    try {
      const result = await ipc.list(parentSessionId, true);
      setNodes(result.items ?? []);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [ipc, parentSessionId]);

  useEffect(() => {
    void reload();
  }, [reload, subagentEventCount]);

  const hasActive = nodes.some((node) => ACTIVE_STATUSES.has(node.status));
  useEffect(() => {
    if (!hasActive) {
      return;
    }
    const timer = window.setInterval(() => {
      void reload();
    }, 1_500);
    return () => window.clearInterval(timer);
  }, [hasActive, reload]);

  const resolve = useCallback(
    async (childId: string, decision: "accept" | "reject" | "rerun") => {
      setBusyChild(childId);
      try {
        await ipc.resolve(childId, decision);
        await reload();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusyChild(null);
      }
    },
    [ipc, reload],
  );

  const cancel = useCallback(
    async (childId: string) => {
      setBusyChild(childId);
      try {
        await ipc.cancel(childId, "user");
        await reload();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusyChild(null);
      }
    },
    [ipc, reload],
  );

  const loadDetails = useCallback(
    async (childId: string) => {
      setBusyChild(childId);
      try {
        let offset = 0;
        let text = "";
        for (let page = 0; page < 4; page += 1) {
          const chunk = await ipc.resultGet(parentSessionId, childId, offset);
          text += chunk.result_chunk;
          if (chunk.next_offset === null) {
            break;
          }
          offset = chunk.next_offset;
        }
        setDetails((current) => ({ ...current, [childId]: text }));
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusyChild(null);
      }
    },
    [ipc, parentSessionId],
  );

  const spawnReview = useCallback(async () => {
    setSpawnBusy(true);
    try {
      await ipc.spawn({
        parent_session_id: parentSessionId,
        role: "review",
        purpose: "独立审查当前会话的变更与风险，输出结构化 findings。",
        template: "adversarial_review",
      });
      await reload();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSpawnBusy(false);
    }
  }, [ipc, parentSessionId, reload]);

  return (
    <section className="subagent-panel" data-testid="subagent-tree" data-parent-session-id={parentSessionId}>
      <header className="subagent-header">
        <span>子 Agent</span>
        <button
          type="button"
          data-action="spawn-review"
          disabled={spawnBusy}
          onClick={() => void spawnReview()}
        >
          派生审查
        </button>
      </header>
      {error ? (
        <p className="subagent-error" role="alert">
          {error}
        </p>
      ) : null}
      {nodes.length === 0 ? (
        <p className="subagent-empty">暂无子 Agent</p>
      ) : (
        <ul className="subagent-list">
          {nodes.map((node) => {
            const pending = node.injection_state === "pending";
            const detail = details[node.child_session_id];
            return (
              <li
                key={node.child_session_id}
                className="subagent-node"
                data-testid="subagent-node"
                data-depth={node.depth}
                data-status={node.status}
                data-verdict={node.verdict ?? ""}
                data-injection-state={node.injection_state}
                data-delivery-mode={node.delivery_mode}
                style={{ marginLeft: node.depth > 1 ? `${(node.depth - 1) * 16}px` : undefined }}
              >
                <button
                  type="button"
                  className="subagent-open"
                  onClick={() => onOpenChild?.(node.child_session_id)}
                >
                  <span className="subagent-title">{node.title}</span>
                  <span className="subagent-meta">
                    {node.role} · {node.status}
                    {node.verdict ? ` · ${SUBAGENT_VERDICT_LABELS[node.verdict] ?? node.verdict}` : ""}
                  </span>
                </button>
                <div className="subagent-result" data-testid="subagent-result">
                  <span
                    className="subagent-state"
                    data-state={node.injection_state}
                    data-delivery-mode={node.delivery_mode}
                  >
                    {SUBAGENT_INJECTION_LABELS[node.injection_state]}
                  </span>
                  {node.result_summary ? (
                    <p className="subagent-summary">
                      {node.result_summary}
                      {node.summary_truncated ? "…" : ""}
                    </p>
                  ) : null}
                  <div className="subagent-actions">
                    <button
                      type="button"
                      data-testid="subagent-result-action"
                      data-action="accept"
                      disabled={!pending || busyChild === node.child_session_id}
                      onClick={() => void resolve(node.child_session_id, "accept")}
                    >
                      接受
                    </button>
                    <button
                      type="button"
                      data-testid="subagent-result-action"
                      data-action="reject"
                      disabled={!pending || busyChild === node.child_session_id}
                      onClick={() => void resolve(node.child_session_id, "reject")}
                    >
                      拒绝
                    </button>
                    <button
                      type="button"
                      data-testid="subagent-result-action"
                      data-action="rerun"
                      disabled={busyChild === node.child_session_id}
                      onClick={() => void resolve(node.child_session_id, "rerun")}
                    >
                      重跑
                    </button>
                    {ACTIVE_STATUSES.has(node.status) ? (
                      <button
                        type="button"
                        data-testid="subagent-result-action"
                        data-action="cancel"
                        disabled={busyChild === node.child_session_id}
                        onClick={() => void cancel(node.child_session_id)}
                      >
                        取消
                      </button>
                    ) : null}
                    <button
                      type="button"
                      data-testid="subagent-result-action"
                      data-action="details"
                      disabled={!node.has_more_details || busyChild === node.child_session_id}
                      onClick={() => void loadDetails(node.child_session_id)}
                    >
                      详情
                    </button>
                  </div>
                  {detail ? (
                    <pre className="subagent-detail" data-role="subagent-result-detail">
                      {detail}
                    </pre>
                  ) : null}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
