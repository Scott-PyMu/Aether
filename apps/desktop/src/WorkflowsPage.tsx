/**
 * 工作流管理/运行视图（M10-01/ADR-031；UI-UX S-14；无拖拽画布）。
 *
 * - 工作流列表（`workflow-list` / `workflow-item`：`data-workflow-id` / `data-is-template`）；
 * - JSON 定义编辑/导入（`workflow-editor` / `workflow-editor-validate` / `workflow-import`）；
 * - 运行视图（`workflow-run-view` / `workflow-node-status` / `workflow-run-cancel` /
 *   `workflow-node-retry`）；轮询运行状态直到终态。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { describeIpcError, ipcErrorCode } from "./startup";
import {
  workflowIpc as productionWorkflowIpc,
  type WorkflowDetail,
  type WorkflowIpc,
  type WorkflowNodeStatus,
  type WorkflowRunDetail,
  type WorkflowSummary,
} from "./workflows";

export interface WorkflowsPageProps {
  /** IPC 契约（缺省 = 生产实现；测试注入替身）。 */
  ipc?: WorkflowIpc;
  /** 返回工作台。 */
  onBack: () => void;
  /** 当前锚点会话（运行工作流需要 session_id）。 */
  sessionId?: string;
}

const ACTIVE_STATUSES = new Set(["queued", "running"]);
const RETRYABLE_STATUSES = new Set(["failed", "timeout", "cancelled"]);

const SAMPLE_DEFINITION = `{
  "schema_version": 1,
  "name": "review-then-summarize",
  "nodes": [
    { "id": "start", "type": "task", "handler": "noop" },
    { "id": "fan", "type": "parallel", "fanout": 2, "max_parallel": 2 },
    { "id": "a", "type": "task", "handler": "noop" },
    { "id": "b", "type": "task", "handler": "noop" },
    { "id": "join", "type": "join", "mode": "all" },
    { "id": "summary", "type": "task", "handler": "emit_summary" }
  ],
  "edges": [
    { "from": "start", "to": "fan" },
    { "from": "fan", "to": "a" },
    { "from": "fan", "to": "b" },
    { "from": "a", "to": "join" },
    { "from": "b", "to": "join" },
    { "from": "join", "to": "summary" }
  ],
  "settings": { "fail_policy": "fail_fast", "timeout_ms": 1800000 }
}`;

interface ValidationState {
  valid: boolean;
  code: string | null;
  message: string | null;
}

function validateLocally(text: string): ValidationState {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    return {
      valid: false,
      code: "invalid_json",
      message: error instanceof Error ? error.message : "JSON 解析失败",
    };
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return { valid: false, code: "invalid_shape", message: "定义必须是 JSON 对象" };
  }
  const object = value as Record<string, unknown>;
  if (object.schema_version !== 1) {
    return { valid: false, code: "schema_version", message: "schema_version 必须为 1" };
  }
  if (!Array.isArray(object.nodes) || object.nodes.length === 0) {
    return { valid: false, code: "invalid_value", message: "nodes 必须是非空数组" };
  }
  if (!Array.isArray(object.edges)) {
    return { valid: false, code: "invalid_value", message: "edges 必须是数组" };
  }
  return { valid: true, code: null, message: null };
}

function statusLabel(status: string): string {
  switch (status) {
    case "queued":
      return "排队中";
    case "running":
      return "运行中";
    case "succeeded":
      return "成功";
    case "failed":
      return "失败";
    case "cancelled":
      return "已取消";
    case "interrupted":
      return "已中断（等待人工重试）";
    case "skipped":
      return "已跳过";
    case "timeout":
      return "超时";
    default:
      return status;
  }
}

export function WorkflowsPage({
  ipc = productionWorkflowIpc,
  onBack,
  sessionId,
}: WorkflowsPageProps) {
  const [items, setItems] = useState<WorkflowSummary[]>([]);
  const [selected, setSelected] = useState<WorkflowDetail | null>(null);
  const [editorText, setEditorText] = useState(SAMPLE_DEFINITION);
  const [validation, setValidation] = useState<ValidationState | null>(null);
  const [importPath, setImportPath] = useState("");
  const [runSessionId, setRunSessionId] = useState(sessionId ?? "");
  const [runInput, setRunInput] = useState("{}");
  const [run, setRun] = useState<WorkflowRunDetail | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);

  const resetError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  const load = useCallback(async () => {
    try {
      const result = await ipc.list();
      setItems(result.items);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    }
  }, [ipc]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (!run || !ACTIVE_STATUSES.has(run.status)) {
      return;
    }
    let active = true;
    const timer = window.setInterval(() => {
      void ipc
        .runGet(run.workflow_run_id, true)
        .then((next) => {
          if (active) {
            setRun(next);
          }
        })
        .catch((failure) => {
          if (active) {
            setError(describeIpcError(failure));
            setErrorCode(ipcErrorCode(failure));
          }
        });
    }, 1000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, [ipc, run]);

  const selectWorkflow = useCallback(
    async (workflowId: string) => {
      resetError();
      setBusy(true);
      try {
        const detail = await ipc.get(workflowId);
        setSelected(detail);
        setEditorText(JSON.stringify(detail.definition, null, 2));
        setValidation({ valid: true, code: null, message: null });
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc, resetError],
  );

  const validate = useCallback(() => {
    const result = validateLocally(editorText);
    setValidation(result);
    return result;
  }, [editorText]);

  const save = useCallback(async () => {
    resetError();
    const result = validate();
    if (!result.valid) {
      return;
    }
    setBusy(true);
    try {
      const definition = JSON.parse(editorText) as unknown;
      if (selected) {
        const updated = await ipc.update({
          workflow_id: selected.workflow_id,
          expected_version: selected.version,
          definition,
        });
        setSelected(updated);
        setEditorText(JSON.stringify(updated.definition, null, 2));
      } else {
        const name =
          (definition as { name?: string }).name ?? `workflow-${Date.now()}`;
        const created = await ipc.create({ name, definition });
        const detail = await ipc.get(created.workflow_id);
        setSelected(detail);
      }
      await load();
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [editorText, ipc, load, resetError, selected, validate]);

  const importDefinition = useCallback(async () => {
    resetError();
    if (!importPath.trim()) {
      setValidation({ valid: false, code: "invalid_value", message: "请填写导入路径" });
      return;
    }
    setBusy(true);
    try {
      const detail = await ipc.importWorkflow(importPath.trim());
      setSelected(detail);
      setEditorText(JSON.stringify(detail.definition, null, 2));
      setValidation({ valid: true, code: null, message: null });
      await load();
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [importPath, ipc, load, resetError]);

  const startRun = useCallback(async () => {
    resetError();
    if (!selected) {
      setValidation({ valid: false, code: "invalid_value", message: "请先选择工作流" });
      return;
    }
    if (!runSessionId.trim()) {
      setValidation({ valid: false, code: "invalid_value", message: "请填写锚点会话 id" });
      return;
    }
    let input: unknown = null;
    try {
      input = runInput.trim() ? JSON.parse(runInput) : null;
    } catch {
      setValidation({ valid: false, code: "invalid_json", message: "运行输入不是合法 JSON" });
      return;
    }
    setBusy(true);
    try {
      const started = await ipc.run({
        workflow_id: selected.workflow_id,
        session_id: runSessionId.trim(),
        input,
      });
      const detail = await ipc.runGet(started.workflow_run_id, true);
      setRun(detail);
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [ipc, resetError, runInput, runSessionId, selected]);

  const cancelRun = useCallback(async () => {
    if (!run) {
      return;
    }
    resetError();
    setBusy(true);
    try {
      await ipc.cancel(run.workflow_run_id, "user");
      setRun(await ipc.runGet(run.workflow_run_id, true));
    } catch (failure) {
      setError(describeIpcError(failure));
      setErrorCode(ipcErrorCode(failure));
    } finally {
      setBusy(false);
    }
  }, [ipc, resetError, run]);

  const retryNode = useCallback(
    async (node: WorkflowNodeStatus) => {
      if (!run) {
        return;
      }
      resetError();
      setBusy(true);
      try {
        await ipc.retryNode(run.workflow_run_id, node.node_id, true);
        setRun(await ipc.runGet(run.workflow_run_id, true));
      } catch (failure) {
        setError(describeIpcError(failure));
        setErrorCode(ipcErrorCode(failure));
      } finally {
        setBusy(false);
      }
    },
    [ipc, resetError, run],
  );

  const nodes = useMemo(() => run?.nodes ?? [], [run]);
  const active = run ? ACTIVE_STATUSES.has(run.status) : false;

  return (
    <div className="overlay" data-testid="overlay-workflows">
      <section
        className="workflow-page"
        data-testid="workflow-page"
        role="dialog"
        aria-modal="true"
      >
        <header className="workflow-header">
          <h2>工作流编排</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            返回工作台
          </button>
        </header>

        {error ? (
          <p className="workflow-error" data-testid="workflow-error" data-error-code={errorCode ?? ""}>
            {error}
          </p>
        ) : null}

        <div className="workflow-layout">
          <aside className="workflow-list" data-testid="workflow-list">
            <button type="button" disabled={busy} onClick={() => void load()}>
              刷新列表
            </button>
            <ul>
              {items.map((item) => (
                <li key={item.workflow_id}>
                  <button
                    type="button"
                    data-testid="workflow-item"
                    data-workflow-id={item.workflow_id}
                    data-is-template={item.is_template ? "true" : "false"}
                    aria-pressed={selected?.workflow_id === item.workflow_id}
                    onClick={() => void selectWorkflow(item.workflow_id)}
                  >
                    {item.name}（v{item.version}
                    {item.is_template ? "，模板" : ""}）
                  </button>
                </li>
              ))}
            </ul>
            {items.length === 0 ? <p>暂无工作流。粘贴 JSON 定义后保存。</p> : null}
          </aside>

          <div className="workflow-editor-panel">
            <label>
              导入 JSON 路径
              <input
                type="text"
                value={importPath}
                onChange={(event) => setImportPath(event.target.value)}
                placeholder={"E:\\path\\workflow.json"}
              />
            </label>
            <button
              type="button"
              data-testid="workflow-import"
              disabled={busy}
              onClick={() => void importDefinition()}
            >
              导入
            </button>
            <textarea
              className="workflow-editor"
              data-testid="workflow-editor"
              value={editorText}
              onChange={(event) => {
                setEditorText(event.target.value);
                setValidation(null);
              }}
              rows={18}
              spellCheck={false}
            />
            <div className="workflow-actions">
              <button
                type="button"
                data-testid="workflow-editor-validate"
                data-valid={validation ? String(validation.valid) : "false"}
                data-error-code={validation?.code ?? ""}
                disabled={busy}
                onClick={() => validate()}
              >
                校验
              </button>
              <button type="button" disabled={busy} onClick={() => void save()}>
                {selected ? "保存新版本" : "创建"}
              </button>
              {validation && !validation.valid ? (
                <span role="alert">{validation.message}</span>
              ) : null}
            </div>
            <div className="workflow-run-actions">
              <label>
                锚点会话 id
                <input
                  type="text"
                  value={runSessionId}
                  onChange={(event) => setRunSessionId(event.target.value)}
                  placeholder="26 位 ULID"
                />
              </label>
              <label>
                运行输入（JSON）
                <textarea
                  value={runInput}
                  onChange={(event) => setRunInput(event.target.value)}
                  rows={3}
                />
              </label>
              <button type="button" disabled={busy || !selected} onClick={() => void startRun()}>
                运行
              </button>
            </div>
          </div>

          <div
            className="workflow-run-view"
            data-testid="workflow-run-view"
            data-run-id={run?.workflow_run_id ?? ""}
            data-status={run?.status ?? ""}
          >
            <h3>运行状态</h3>
            {!run ? (
              <p>尚未运行。</p>
            ) : (
              <>
                <p>
                  {run.workflow_run_id} — {statusLabel(run.status)}
                  {run.error ? `（${run.error}）` : ""}
                </p>
                <button
                  type="button"
                  data-testid="workflow-run-cancel"
                  disabled={busy || !active}
                  onClick={() => void cancelRun()}
                >
                  取消运行
                </button>
                <ul className="workflow-node-list">
                  {nodes.map((node) => (
                    <li
                      key={node.node_run_id}
                      data-testid="workflow-node-status"
                      data-node-id={node.node_id}
                      data-attempt={node.attempt}
                      data-status={node.status}
                    >
                      <span>
                        {node.node_id}（{node.type} / #{node.attempt}）— {statusLabel(node.status)}
                      </span>
                      {node.error ? <span className="workflow-node-error">{node.error}</span> : null}
                      {RETRYABLE_STATUSES.has(node.status) ? (
                        <button
                          type="button"
                          data-testid="workflow-node-retry"
                          disabled={busy || active}
                          onClick={() => void retryNode(node)}
                        >
                          重试节点
                        </button>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </>
            )}
          </div>
        </div>
      </section>
    </div>
  );
}
