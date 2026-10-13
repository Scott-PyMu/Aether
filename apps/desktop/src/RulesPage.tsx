/**
 * 权限规则管理页（M6-01；ADR-019 / ADR-018；UI-UX §7.3「规则」锚点）。
 *
 * - 列表（`rules_list`）：每条规则锚点 `rule-item` + `data-rule-id` / `data-enabled`；
 * - 启停（`rule_toggle`，标题带状态）/ 删除（`rule_delete` + 二次确认）；
 * - 新增（`rule_add`）：`action` 由 `resource` 推导（fs.read→read、fs.write→write、
 *   exec→exec、net→connect）；`target_kind` 与 `resource` 联动（fs.* → path/any、
 *   exec → command/any、net → host/any）；`any` 时 `target` 省略（表单字段禁用）；
 * - 错误呈现沿用既有模式（错误元素 `data-code`，UI-UX §7.3 通用行）；
 *   `rule_not_found` → 「规则不存在，已刷新列表」且自动重取 `rules_list`。
 */
import { useCallback, useEffect, useState } from "react";

import {
  RULE_DECISIONS,
  RULE_DECISION_LABELS,
  RULE_NOT_FOUND_CODE,
  RULE_NOT_FOUND_MESSAGE,
  RULE_RESOURCE_LABELS,
  RULE_RESOURCES,
  RULE_TARGET_KIND_LABELS,
  ruleActionForResource,
  ruleTargetKindsForResource,
  rulesIpc as productionRulesIpc,
  type RuleAddInput,
  type RuleEntry,
  type RuleResource,
  type RulesIpc,
} from "./rules";
import { describeIpcError, ipcErrorCode } from "./startup";

export interface RulesPageProps {
  /** 规则命令面（测试/E2E 注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: RulesIpc;
}

export function RulesPage({ ipc = productionRulesIpc }: RulesPageProps) {
  const [rules, setRules] = useState<RuleEntry[] | null>(null);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // 新增表单草稿（`resource` 决定 `action` 与可选 `target_kind`）。
  const [name, setName] = useState("");
  const [resource, setResource] = useState<RuleResource>("fs.read");
  const [targetKind, setTargetKind] = useState<string>("path");
  const [target, setTarget] = useState("");
  const [decision, setDecision] = useState<string>("allow");
  const [priority, setPriority] = useState("0");

  const reportError = useCallback((failure: unknown) => {
    setError(describeIpcError(failure));
    setErrorCode(ipcErrorCode(failure));
  }, []);
  const clearError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  /** 刷新清单；`rule_not_found` 路径复用（自动重取）。 */
  const refresh = useCallback(async () => {
    try {
      const list = await ipc.listRules();
      setRules(list);
      clearError();
    } catch (failure) {
      setRules([]);
      reportError(failure);
    }
  }, [clearError, ipc, reportError]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** 错误收口：`rule_not_found` → 登记文案 + 自动刷新；其余走通用结构化错误。 */
  const handleRuleError = useCallback(
    async (failure: unknown) => {
      if (ipcErrorCode(failure) === RULE_NOT_FOUND_CODE) {
        setError(RULE_NOT_FOUND_MESSAGE);
        setErrorCode(RULE_NOT_FOUND_CODE);
        await refresh();
        return;
      }
      reportError(failure);
    },
    [refresh, reportError],
  );

  const onToggle = useCallback(
    async (rule: RuleEntry) => {
      setBusy(true);
      clearError();
      setNotice(null);
      try {
        await ipc.toggleRule(rule.id, !rule.enabled);
        await refresh();
      } catch (failure) {
        await handleRuleError(failure);
      } finally {
        setBusy(false);
      }
    },
    [clearError, handleRuleError, ipc, refresh],
  );

  const onConfirmDelete = useCallback(
    async (ruleId: string) => {
      setBusy(true);
      clearError();
      setNotice(null);
      try {
        await ipc.deleteRule(ruleId);
        setConfirmDeleteId(null);
        setNotice("规则已删除");
        await refresh();
      } catch (failure) {
        setConfirmDeleteId(null);
        await handleRuleError(failure);
      } finally {
        setBusy(false);
      }
    },
    [clearError, handleRuleError, ipc, refresh],
  );

  const onAdd = useCallback(async () => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      setError("请填写规则名称");
      setErrorCode("missing_field");
      return;
    }
    const trimmedTarget = target.trim();
    if (targetKind !== "any" && !trimmedTarget) {
      setError("请填写规则目标（`any` 档可留空）");
      setErrorCode("missing_field");
      return;
    }
    const priorityDraft = priority.trim();
    const priorityValue = priorityDraft === "" ? undefined : Number(priorityDraft);
    if (
      priorityValue !== undefined &&
      (!Number.isInteger(priorityValue) || priorityValue < 0 || priorityValue > 100)
    ) {
      setError("优先级必须是 0–100 的整数");
      setErrorCode("out_of_range");
      return;
    }
    setBusy(true);
    clearError();
    setNotice(null);
    try {
      const input: RuleAddInput = {
        name: trimmedName,
        resource,
        action: ruleActionForResource(resource),
        target_kind: targetKind,
        decision,
        ...(targetKind !== "any" ? { target: trimmedTarget } : {}),
        ...(priorityValue !== undefined ? { priority: priorityValue } : {}),
      };
      await ipc.addRule(input);
      setName("");
      setTarget("");
      setPriority("0");
      setNotice("规则已创建");
      await refresh();
    } catch (failure) {
      reportError(failure);
    } finally {
      setBusy(false);
    }
  }, [clearError, decision, ipc, name, priority, refresh, reportError, resource, target, targetKind]);

  /** `resource` 切换时联动 `target_kind`（非法保留值回退到首个允许值）。 */
  const onSelectResource = useCallback((next: RuleResource) => {
    setResource(next);
    const kinds = ruleTargetKindsForResource(next);
    setTargetKind((current) => (kinds.includes(current) ? current : (kinds[0] ?? "any")));
  }, []);

  const targetKinds = ruleTargetKindsForResource(resource);
  const targetDisabled = targetKind === "any";

  return (
    <section className="rules-page" data-testid="rules-page">
      <div className="rules-head">
        <h3>权限规则</h3>
        <p className="rules-desc">
          规则在档位矩阵之上叠加：`deny` 收紧恒允许、`allow` 免逐次审批；规则不得越过档位硬边界
          （`read_only` 写、低档 `exec`/`net` 不因规则放宽）。
        </p>
      </div>

      {rules === null ? (
        <p className="rules-loading">正在加载规则…</p>
      ) : rules.length === 0 ? (
        <p className="rules-empty" data-testid="rules-empty">
          暂无规则
        </p>
      ) : (
        <ul className="rules-list">
          {rules.map((rule) => (
            <li
              key={rule.id}
              className="rule-item"
              data-testid="rule-item"
              data-rule-id={rule.id}
              data-enabled={String(rule.enabled)}
            >
              <span className="rule-name" data-testid="rule-name">
                {rule.name}
              </span>
              <span className="rule-summary" data-testid="rule-summary">
                {rule.resource}:{rule.action} · {rule.target_kind}
                {rule.target ? ` · ${rule.target}` : ""} · {rule.decision} · 优先级 {rule.priority}
              </span>
              {confirmDeleteId === rule.id ? (
                <div className="rule-confirm" data-testid="rule-delete-confirm">
                  <span className="rule-confirm-text">确认删除？</span>
                  <button
                    type="button"
                    data-testid="rule-delete-cancel"
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(null)}
                  >
                    取消
                  </button>
                  <button
                    type="button"
                    data-testid="rule-delete-confirm-button"
                    disabled={busy}
                    onClick={() => void onConfirmDelete(rule.id)}
                  >
                    删除
                  </button>
                </div>
              ) : (
                <div className="rule-actions">
                  <button
                    type="button"
                    data-testid="rule-toggle"
                    data-rule-id={rule.id}
                    data-enabled={String(rule.enabled)}
                    title={rule.enabled ? "停用规则" : "启用规则"}
                    disabled={busy}
                    onClick={() => void onToggle(rule)}
                  >
                    {rule.enabled ? "停用" : "启用"}
                  </button>
                  <button
                    type="button"
                    data-testid="rule-delete"
                    data-rule-id={rule.id}
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(rule.id)}
                  >
                    删除
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}

      <div className="rule-add-form" data-testid="rule-add-form">
        <h4>新增规则</h4>
        <label className="rule-field">
          名称
          <input
            type="text"
            data-testid="rule-name-input"
            placeholder="规则名称（≤64 字符）"
            value={name}
            disabled={busy}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label className="rule-field">
          资源
          <select
            data-testid="rule-resource-select"
            value={resource}
            disabled={busy}
            onChange={(event) => onSelectResource(event.target.value as RuleResource)}
          >
            {RULE_RESOURCES.map((value) => (
              <option key={value} value={value}>
                {RULE_RESOURCE_LABELS[value]}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          目标类型
          <select
            data-testid="rule-target-kind-select"
            value={targetKind}
            disabled={busy}
            onChange={(event) => setTargetKind(event.target.value)}
          >
            {targetKinds.map((kind) => (
              <option key={kind} value={kind}>
                {RULE_TARGET_KIND_LABELS[kind] ?? kind}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          目标
          <input
            type="text"
            data-testid="rule-target-input"
            placeholder={targetDisabled ? "目标类型为 any 时无需目标" : "路径 / 命令名 / 主机名"}
            value={target}
            disabled={busy || targetDisabled}
            onChange={(event) => setTarget(event.target.value)}
          />
        </label>
        <label className="rule-field">
          决议
          <select
            data-testid="rule-decision-select"
            value={decision}
            disabled={busy}
            onChange={(event) => setDecision(event.target.value)}
          >
            {RULE_DECISIONS.map((value) => (
              <option key={value} value={value}>
                {RULE_DECISION_LABELS[value] ?? value}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          优先级（0–100）
          <input
            type="number"
            min={0}
            max={100}
            data-testid="rule-priority-input"
            value={priority}
            disabled={busy}
            onChange={(event) => setPriority(event.target.value)}
          />
        </label>
        <div className="rule-add-actions">
          <button
            type="button"
            data-testid="rule-add-submit"
            disabled={busy}
            onClick={() => void onAdd()}
          >
            添加规则
          </button>
        </div>
      </div>

      {notice ? (
        <p className="rules-notice" data-testid="rules-notice" role="status">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          className="rules-error"
          data-testid="rules-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {error}
        </p>
      ) : null}
    </section>
  );
}
