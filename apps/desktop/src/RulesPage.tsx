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
import { t, tKeyOrRaw } from "./i18n";

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
        setError(t(RULE_NOT_FOUND_MESSAGE));
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
        setNotice(t("rules.rule_deleted"));
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
      setError(t("rules.enter_a_rule_name"));
      setErrorCode("missing_field");
      return;
    }
    const trimmedTarget = target.trim();
    if (targetKind !== "any" && !trimmedTarget) {
      setError(t("rules.enter_a_rule_target_can"));
      setErrorCode("missing_field");
      return;
    }
    const priorityDraft = priority.trim();
    const priorityValue = priorityDraft === "" ? undefined : Number(priorityDraft);
    if (
      priorityValue !== undefined &&
      (!Number.isInteger(priorityValue) || priorityValue < 0 || priorityValue > 100)
    ) {
      setError(t("rules.priority_must_be_an_integer"));
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
      setNotice(t("rules.rule_created"));
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
        <h3>{t("app.common.permission_rules")}</h3>
        <p className="rules-desc">
          {t("rules.rules_layer_on_top_of")}
        </p>
      </div>

      {rules === null ? (
        <p className="rules-loading">{t("rules.loading_rules")}</p>
      ) : rules.length === 0 ? (
        <p className="rules-empty" data-testid="rules-empty">
          {t("rules.no_rules")}
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
                {rule.target ? ` · ${rule.target}` : ""} · {rule.decision} {t("rules.priority")} {rule.priority}
              </span>
              {confirmDeleteId === rule.id ? (
                <div className="rule-confirm" data-testid="rule-delete-confirm">
                  <span className="rule-confirm-text">{t("app.action.delete_2")}</span>
                  <button
                    type="button"
                    data-testid="rule-delete-cancel"
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(null)}
                  >
                    {t("app.action.cancel")}
                  </button>
                  <button
                    type="button"
                    data-testid="rule-delete-confirm-button"
                    disabled={busy}
                    onClick={() => void onConfirmDelete(rule.id)}
                  >
                    {t("app.action.delete")}
                  </button>
                </div>
              ) : (
                <div className="rule-actions">
                  <button
                    type="button"
                    data-testid="rule-toggle"
                    data-rule-id={rule.id}
                    data-enabled={String(rule.enabled)}
                    title={rule.enabled ? t("rules.disable_rule") : t("rules.enable_rule")}
                    disabled={busy}
                    onClick={() => void onToggle(rule)}
                  >
                    {rule.enabled ? t("app.action.disable") : t("app.action.enable")}
                  </button>
                  <button
                    type="button"
                    data-testid="rule-delete"
                    data-rule-id={rule.id}
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(rule.id)}
                  >
                    {t("app.action.delete")}
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}

      <div className="rule-add-form" data-testid="rule-add-form">
        <h4>{t("rules.add_rule")}</h4>
        <label className="rule-field">
          {t("app.common.name")}
          <input
            type="text"
            data-testid="rule-name-input"
            placeholder={t("rules.rule_name_64_characters")}
            value={name}
            disabled={busy}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label className="rule-field">
          {t("rules.resource")}
          <select
            data-testid="rule-resource-select"
            value={resource}
            disabled={busy}
            onChange={(event) => onSelectResource(event.target.value as RuleResource)}
          >
            {RULE_RESOURCES.map((value) => (
              <option key={value} value={value}>
                {t(RULE_RESOURCE_LABELS[value])}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          {t("rules.target_type")}
          <select
            data-testid="rule-target-kind-select"
            value={targetKind}
            disabled={busy}
            onChange={(event) => setTargetKind(event.target.value)}
          >
            {targetKinds.map((kind) => (
              <option key={kind} value={kind}>
                {tKeyOrRaw(RULE_TARGET_KIND_LABELS[kind] ?? kind)}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          {t("rules.target")}
          <input
            type="text"
            data-testid="rule-target-input"
            placeholder={targetDisabled ? t("rules.no_target_is_needed_when") : t("rules.path_command_name_host_name")}
            value={target}
            disabled={busy || targetDisabled}
            onChange={(event) => setTarget(event.target.value)}
          />
        </label>
        <label className="rule-field">
          {t("rules.decision")}
          <select
            data-testid="rule-decision-select"
            value={decision}
            disabled={busy}
            onChange={(event) => setDecision(event.target.value)}
          >
            {RULE_DECISIONS.map((value) => (
              <option key={value} value={value}>
                {tKeyOrRaw(RULE_DECISION_LABELS[value] ?? value)}
              </option>
            ))}
          </select>
        </label>
        <label className="rule-field">
          {t("rules.priority_0_100")}
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
            {t("rules.add_rule_2")}
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
