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
import { t } from "./i18n";

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
      message: error instanceof Error ? error.message : t("workflow.json_parse_failed"),
    };
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return { valid: false, code: "invalid_shape", message: t("workflow.definition_must_be_a_json") };
  }
  const object = value as Record<string, unknown>;
  if (object.schema_version !== 1) {
    return { valid: false, code: "schema_version", message: t("workflow.schema_version_must_be_1") };
  }
  if (!Array.isArray(object.nodes) || object.nodes.length === 0) {
    return { valid: false, code: "invalid_value", message: t("workflow.nodes_must_be_a_non") };
  }
  if (!Array.isArray(object.edges)) {
    return { valid: false, code: "invalid_value", message: t("workflow.edges_must_be_an_array") };
  }
  return { valid: true, code: null, message: null };
}

function statusLabel(status: string): string {
  switch (status) {
    case "queued":
      return t("workflow.queued");
    case "running":
      return t("app.common.running");
    case "succeeded":
      return t("app.common.success");
    case "failed":
      return t("app.common.failed");
    case "cancelled":
      return t("app.common.cancelled");
    case "interrupted":
      return t("workflow.interrupted_awaiting_manual_retry");
    case "skipped":
      return t("workflow.skipped");
    case "timeout":
      return t("app.common.timeout");
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
      setValidation({ valid: false, code: "invalid_value", message: t("workflow.enter_an_import_path") });
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
      setValidation({ valid: false, code: "invalid_value", message: t("workflow.select_a_workflow_first") });
      return;
    }
    if (!runSessionId.trim()) {
      setValidation({ valid: false, code: "invalid_value", message: t("workflow.enter_the_anchor_session_id") });
      return;
    }
    let input: unknown = null;
    try {
      input = runInput.trim() ? JSON.parse(runInput) : null;
    } catch {
      setValidation({ valid: false, code: "invalid_json", message: t("workflow.run_input_is_not_valid") });
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
          <h2>{t("workflow.workflow_orchestration")}</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            {t("app.action.back_to_workbench")}
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
              {t("workflow.refresh_list")}
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
                    {item.name}{t("app.punct.open_paren_v")}{item.version}
                    {item.is_template ? t("workflow.template") : ""}{t("app.punct.close_paren")}
                  </button>
                </li>
              ))}
            </ul>
            {items.length === 0 ? <p>{t("workflow.no_workflows_paste_a_json")}</p> : null}
          </aside>

          <div className="workflow-editor-panel">
            <label>
              {t("workflow.import_json_path")}
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
              {t("workflow.import")}
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
                {t("workflow.validate")}
              </button>
              <button type="button" disabled={busy} onClick={() => void save()}>
                {selected ? t("workflow.save_new_version") : t("app.action.create")}
              </button>
              {validation && !validation.valid ? (
                <span role="alert">{validation.message}</span>
              ) : null}
            </div>
            <div className="workflow-run-actions">
              <label>
                {t("workflow.anchor_session_id")}
                <input
                  type="text"
                  value={runSessionId}
                  onChange={(event) => setRunSessionId(event.target.value)}
                  placeholder={t("workflow.26_character_ulid")}
                />
              </label>
              <label>
                {t("workflow.run_input_json")}
                <textarea
                  value={runInput}
                  onChange={(event) => setRunInput(event.target.value)}
                  rows={3}
                />
              </label>
              <button type="button" disabled={busy || !selected} onClick={() => void startRun()}>
                {t("workflow.run")}
              </button>
            </div>
          </div>

          <div
            className="workflow-run-view"
            data-testid="workflow-run-view"
            data-run-id={run?.workflow_run_id ?? ""}
            data-status={run?.status ?? ""}
          >
            <h3>{t("workflow.run_status")}</h3>
            {!run ? (
              <p>{t("workflow.not_run_yet")}</p>
            ) : (
              <>
                <p>
                  {run.workflow_run_id} — {statusLabel(run.status)}
                  {run.error ? t("workflow.text", { p0: run.error }) : ""}
                </p>
                <button
                  type="button"
                  data-testid="workflow-run-cancel"
                  disabled={busy || !active}
                  onClick={() => void cancelRun()}
                >
                  {t("workflow.cancel_run")}
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
                        {node.node_id}{t("app.punct.open_paren")}{node.type} / #{node.attempt}{t("app.punct.close_paren_dash")} {statusLabel(node.status)}
                      </span>
                      {node.error ? <span className="workflow-node-error">{node.error}</span> : null}
                      {RETRYABLE_STATUSES.has(node.status) ? (
                        <button
                          type="button"
                          data-testid="workflow-node-retry"
                          disabled={busy || active}
                          onClick={() => void retryNode(node)}
                        >
                          {t("workflow.retry_node")}
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
