/**
 * 模型与供应商配置页（M3-11；ADR-010 决策 3/4；S-05 设置覆盖层内页面）。
 *
 * - 供应商列表：卡片（图标/名称/描述「N 个模型已启用」）+ 启用开关
 *   （`provider_toggle`）+ 编辑/删除（删除二次确认；内置删除入口置灰）；
 * - 新建：`添加供应商`（官方预设，预填名称与 Base URL）/ `添加自定义供应商`
 *   （`type=custom`，Base URL 必填）；
 * - 表单：名称 / Base URL（请求路径预览）/ API Key（明文输入、掩码 + 显示/隐藏；
 *   仅传输，核心写 keyring 后只存引用）/ 启用开关 / 模型管理（启用/停用 + 手动添加）；
 *   编辑态显示 `api_key_ref` 只读 + 可选覆盖输入；
 * - `测试连接`：P0 无命令——点击显示 toast「连通性测试将在 P1 开放」（无 IPC、无网络）；
 * - `从供应商获取`：P0 不提供入口（P1）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  OFFICIAL_PROVIDER_PRESETS,
  providerAbbr,
  providerColor,
  providerDesc,
  providersIpc as productionProvidersIpc,
  type ProviderEntry,
  type ProviderModel,
  type ProviderType,
  type ProvidersIpc,
} from "./providers";
import { describeIpcError, ipcErrorCode } from "./startup";

/** 测试连接提示（P0 口径；ADR-010 决策 3/§5-7）。 */
export const PROVIDER_TEST_NOTICE = "连通性测试将在 P1 开放";

type FormMode = "official" | "custom";

export interface ProvidersPageProps {
  /** 供应商 IPC（测试/E2E 注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: ProvidersIpc;
}

export function ProvidersPage({ ipc = productionProvidersIpc }: ProvidersPageProps) {
  const [providers, setProviders] = useState<ProviderEntry[] | null>(null);
  const [view, setView] = useState<"list" | "form">("list");
  /** 编辑目标（`null` = 新建）。 */
  const [editing, setEditing] = useState<ProviderEntry | null>(null);
  const [formMode, setFormMode] = useState<FormMode>("official");
  const [formType, setFormType] = useState<ProviderType>("anthropic");
  const [formName, setFormName] = useState("");
  const [formBaseUrl, setFormBaseUrl] = useState("");
  const [formApiKey, setFormApiKey] = useState("");
  const [apiKeyTouched, setApiKeyTouched] = useState(false);
  const [showKey, setShowKey] = useState(false);
  const [formEnabled, setFormEnabled] = useState(true);
  const [modelIdDraft, setModelIdDraft] = useState("");
  const [modelNameDraft, setModelNameDraft] = useState("");
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const reportError = useCallback((failure: unknown) => {
    setError(describeIpcError(failure));
    setErrorCode(ipcErrorCode(failure));
  }, []);
  const clearError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  /** 刷新清单；`editingId` 提供时同步表单中的编辑目标（模型增改后回显）。 */
  const refresh = useCallback(
    async (editingId?: string | null) => {
      try {
        const list = await ipc.list();
        setProviders(list);
        if (editingId) {
          setEditing(list.find((provider) => provider.id === editingId) ?? null);
        }
        clearError();
      } catch (failure) {
        setProviders([]);
        reportError(failure);
      }
    },
    [clearError, ipc, reportError],
  );

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const openCreate = useCallback((mode: FormMode) => {
    setEditing(null);
    setFormMode(mode);
    setConfirmDeleteId(null);
    setNotice(null);
    setToast(null);
    setError(null);
    setErrorCode(null);
    setApiKeyTouched(false);
    setFormApiKey("");
    setShowKey(false);
    setFormEnabled(true);
    setModelIdDraft("");
    setModelNameDraft("");
    if (mode === "official") {
      const preset = OFFICIAL_PROVIDER_PRESETS.anthropic;
      setFormType("anthropic");
      setFormName(preset.name);
      setFormBaseUrl(preset.baseUrl);
    } else {
      setFormType("custom");
      setFormName("");
      setFormBaseUrl("");
    }
    setView("form");
  }, []);

  const openEdit = useCallback((provider: ProviderEntry) => {
    setEditing(provider);
    setFormMode(provider.type === "custom" ? "custom" : "official");
    setFormType(provider.type);
    setFormName(provider.name);
    setFormBaseUrl(provider.base_url ?? "");
    setFormApiKey("");
    setApiKeyTouched(false);
    setShowKey(false);
    setFormEnabled(provider.enabled);
    setModelIdDraft("");
    setModelNameDraft("");
    setConfirmDeleteId(null);
    setNotice(null);
    setToast(null);
    setError(null);
    setErrorCode(null);
    setView("form");
  }, []);

  const backToList = useCallback(() => {
    setView("list");
    setEditing(null);
    setConfirmDeleteId(null);
    setToast(null);
    clearError();
    void refresh();
  }, [clearError, refresh]);

  const onSelectOfficialType = useCallback((type: ProviderType) => {
    setFormType(type);
    if (type !== "custom") {
      const preset = OFFICIAL_PROVIDER_PRESETS[type];
      setFormName(preset.name);
      setFormBaseUrl(preset.baseUrl);
    }
  }, []);

  const onSave = useCallback(async () => {
    const name = formName.trim();
    const baseUrl = formBaseUrl.trim();
    if (!name) {
      setError("请填写供应商名称");
      setErrorCode("invalid_format");
      return;
    }
    if (formMode === "custom" && !editing && !baseUrl) {
      setError("自定义供应商必须填写 Base URL");
      setErrorCode("missing_field");
      return;
    }
    setBusy(true);
    clearError();
    setToast(null);
    try {
      if (editing) {
        const input: Parameters<ProvidersIpc["update"]>[0] = {
          id: editing.id,
          name,
          base_url: baseUrl,
          enabled: formEnabled,
        };
        if (formApiKey) {
          input.api_key = formApiKey;
        } else if (apiKeyTouched) {
          // 空串 = 清除密钥引用 + 自身命名空间 keychain 条目（ADR-010 决策 3 三态）。
          input.api_key = "";
        }
        await ipc.update(input);
        setNotice(`已保存供应商：${name}`);
      } else {
        const input: Parameters<ProvidersIpc["create"]>[0] = {
          name,
          type: formType,
          enabled: formEnabled,
        };
        if (baseUrl) {
          input.base_url = baseUrl;
        }
        if (formApiKey) {
          input.api_key = formApiKey;
        }
        const created = await ipc.create(input);
        setNotice(`已添加供应商：${created.name}`);
      }
      await refresh();
      setApiKeyTouched(false);
      setFormApiKey("");
      setView("list");
    } catch (failure) {
      reportError(failure);
    } finally {
      setBusy(false);
    }
  }, [
    apiKeyTouched,
    clearError,
    editing,
    formApiKey,
    formBaseUrl,
    formEnabled,
    formMode,
    formName,
    formType,
    ipc,
    refresh,
    reportError,
  ]);

  const onToggleProvider = useCallback(
    async (id: string, enabled: boolean) => {
      setBusy(true);
      clearError();
      try {
        await ipc.toggle(id, enabled);
        await refresh();
      } catch (failure) {
        reportError(failure);
      } finally {
        setBusy(false);
      }
    },
    [clearError, ipc, refresh, reportError],
  );

  const onConfirmDelete = useCallback(
    async (id: string) => {
      setBusy(true);
      clearError();
      try {
        await ipc.remove(id);
        setConfirmDeleteId(null);
        setNotice("供应商已删除（模型与密钥引用一并移除）");
        await refresh();
      } catch (failure) {
        setConfirmDeleteId(null);
        reportError(failure);
      } finally {
        setBusy(false);
      }
    },
    [clearError, ipc, refresh, reportError],
  );

  const onAddModel = useCallback(async () => {
    if (!editing) {
      return;
    }
    const modelId = modelIdDraft.trim();
    if (!modelId) {
      setError("请填写模型 ID");
      setErrorCode("invalid_format");
      return;
    }
    setBusy(true);
    clearError();
    try {
      const displayName = modelNameDraft.trim() || modelId;
      await ipc.addModel(editing.id, modelId, displayName);
      setModelIdDraft("");
      setModelNameDraft("");
      await refresh(editing.id);
    } catch (failure) {
      reportError(failure);
    } finally {
      setBusy(false);
    }
  }, [clearError, editing, ipc, modelIdDraft, modelNameDraft, refresh, reportError]);

  const onToggleModel = useCallback(
    async (model: ProviderModel, enabled: boolean) => {
      if (!editing) {
        return;
      }
      setBusy(true);
      clearError();
      try {
        await ipc.toggleModel(editing.id, model.model_id, enabled);
        await refresh(editing.id);
      } catch (failure) {
        reportError(failure);
      } finally {
        setBusy(false);
      }
    },
    [clearError, editing, ipc, refresh, reportError],
  );

  const previewPath = useMemo(() => {
    if (formMode === "official" && formType !== "custom") {
      return OFFICIAL_PROVIDER_PRESETS[formType].preview;
    }
    return formBaseUrl ? `${formBaseUrl}/v1/chat/completions` : "输入 Base URL 后显示请求路径";
  }, [formBaseUrl, formMode, formType]);

  const formTitle = editing
    ? "编辑供应商"
    : formMode === "custom"
      ? "添加自定义供应商"
      : "添加供应商";

  if (view === "form") {
    return (
      <section className="providers-page" data-testid="providers-page">
        <div className="provider-form" data-testid="provider-form">
          <div className="provider-form-head">
            <button type="button" data-testid="provider-form-back" onClick={backToList}>
              返回
            </button>
            <h3>{formTitle}</h3>
            <button
              type="button"
              data-testid="provider-form-save"
              disabled={busy}
              onClick={() => void onSave()}
            >
              {editing ? "保存" : "创建"}
            </button>
          </div>

          {formMode === "official" && !editing ? (
            <label className="provider-field">
              供应商类型
              <select
                data-testid="provider-type-select"
                value={formType}
                disabled={busy}
                onChange={(event) => onSelectOfficialType(event.target.value as ProviderType)}
              >
                {Object.entries(OFFICIAL_PROVIDER_PRESETS).map(([value, preset]) => (
                  <option key={value} value={value}>
                    {preset.name}
                  </option>
                ))}
              </select>
            </label>
          ) : null}

          <label className="provider-field">
            供应商名称
            <input
              type="text"
              data-testid="provider-name-input"
              placeholder="例如: My Anthropic"
              value={formName}
              disabled={busy}
              onChange={(event) => setFormName(event.target.value)}
            />
          </label>

          <label className="provider-field">
            Base URL
            <span className="provider-preview">预览：{previewPath}</span>
            <input
              type="text"
              data-testid="provider-base-url-input"
              placeholder="https://api.example.com"
              value={formBaseUrl}
              disabled={busy}
              onChange={(event) => setFormBaseUrl(event.target.value)}
            />
          </label>

          <div className="provider-field">
            <span className="provider-field-label">
              <span>API Key</span>
              <button
                type="button"
                data-testid="provider-test"
                disabled={busy}
                onClick={() => setToast(PROVIDER_TEST_NOTICE)}
              >
                测试连接
              </button>
            </span>
            {editing?.api_key_ref ? (
              <p className="provider-key-ref" data-testid="provider-api-key-ref-readonly">
                当前密钥引用（只读）：{editing.api_key_ref}
              </p>
            ) : null}
            <div className="provider-key-input">
              <input
                type={showKey ? "text" : "password"}
                data-testid="provider-api-key-input"
                placeholder={editing ? "输入新密钥以覆盖（留空保持不变）" : "输入 API Key"}
                value={formApiKey}
                disabled={busy}
                onChange={(event) => {
                  setFormApiKey(event.target.value);
                  setApiKeyTouched(true);
                }}
              />
              <button
                type="button"
                data-testid="provider-api-key-reveal"
                disabled={busy}
                onClick={() => setShowKey((visible) => !visible)}
              >
                {showKey ? "隐藏" : "显示"}
              </button>
            </div>
            {editing && apiKeyTouched && formApiKey.length === 0 && editing.api_key_ref ? (
              <p className="provider-key-hint">保存后将清除密钥引用（keychain 条目一并删除）</p>
            ) : null}
          </div>

          <label className="provider-enabled-row">
            <input
              type="checkbox"
              data-testid="provider-enabled-switch"
              checked={formEnabled}
              disabled={busy}
              onChange={(event) => setFormEnabled(event.target.checked)}
            />
            启用此配置（关闭后该配置的模型不会出现在选择器中）
          </label>

          {editing ? (
            <section className="provider-models">
              <h4>模型</h4>
              {editing.models.length === 0 ? (
                <p className="provider-model-empty">还没有模型，从下方添加</p>
              ) : (
                <ul className="provider-model-list">
                  {editing.models.map((model) => (
                    <li
                      key={model.id}
                      className="provider-model-item"
                      data-testid="provider-model-item"
                      data-model-id={model.model_id}
                      data-enabled={String(model.enabled)}
                    >
                      <span className="provider-model-name">{model.display_name}</span>
                      <span className="provider-model-id">{model.model_id}</span>
                      <button
                        type="button"
                        data-testid="provider-model-toggle"
                        data-model-id={model.model_id}
                        data-action={model.enabled ? "disable" : "enable"}
                        disabled={busy}
                        onClick={() => void onToggleModel(model, !model.enabled)}
                      >
                        {model.enabled ? "停用" : "启用"}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
              <div className="provider-model-add-row">
                <input
                  type="text"
                  data-testid="provider-model-id-input"
                  placeholder="模型 ID（如 claude-opus-4-6）"
                  value={modelIdDraft}
                  disabled={busy}
                  onChange={(event) => setModelIdDraft(event.target.value)}
                />
                <input
                  type="text"
                  data-testid="provider-model-name-input"
                  placeholder="显示名称（可选）"
                  value={modelNameDraft}
                  disabled={busy}
                  onChange={(event) => setModelNameDraft(event.target.value)}
                />
                <button
                  type="button"
                  data-testid="provider-model-add"
                  disabled={busy}
                  onClick={() => void onAddModel()}
                >
                  ＋ 添加模型
                </button>
              </div>
            </section>
          ) : (
            <p className="provider-models-hint">创建保存后可管理模型</p>
          )}

          {toast ? (
            <p className="provider-notice" data-testid="provider-test-notice" role="status">
              {toast}
            </p>
          ) : null}
          {error ? (
            <p
              className="provider-error"
              data-testid="provider-error"
              data-code={errorCode ?? ""}
              role="alert"
            >
              {errorCode === "builtin_provider_undeletable"
                ? "内置供应商不可删除"
                : error}
            </p>
          ) : null}
        </div>
      </section>
    );
  }

  return (
    <section className="providers-page" data-testid="providers-page">
      <div className="providers-head">
        <div>
          <h3>模型配置</h3>
          <p className="providers-desc">
            管理 AI 供应商连接，配置 API Key 和可用模型。启用的供应商与模型会出现在会话输入框的模型选择器中。
          </p>
        </div>
        <div className="providers-actions">
          <button
            type="button"
            data-testid="providers-add"
            disabled={busy}
            onClick={() => openCreate("official")}
          >
            ＋ 添加供应商
          </button>
          <button
            type="button"
            data-testid="providers-add-custom"
            disabled={busy}
            onClick={() => openCreate("custom")}
          >
            ＋ 添加自定义供应商
          </button>
        </div>
      </div>

      {providers === null ? (
        <p className="providers-loading">正在加载供应商…</p>
      ) : providers.length === 0 ? (
        <p className="providers-empty" data-testid="provider-empty">
          还没有供应商配置，点击右上角添加
        </p>
      ) : (
        <ul className="provider-cards">
          {providers.map((provider) => (
            <li
              key={provider.id}
              className="provider-card"
              data-testid="provider-card"
              data-provider-id={provider.id}
              data-enabled={String(provider.enabled)}
              data-is-builtin={String(provider.is_builtin)}
            >
              <span
                className="provider-icon"
                style={{ background: providerColor(provider.type) }}
              >
                {providerAbbr(provider)}
              </span>
              <div className="provider-info">
                <div className="provider-name">{provider.name}</div>
                <div className="provider-desc">{providerDesc(provider)}</div>
              </div>
              {confirmDeleteId === provider.id ? (
                <div className="provider-confirm" data-testid="provider-delete-confirm">
                  <span className="provider-confirm-text">确认删除？</span>
                  <button
                    type="button"
                    data-testid="provider-delete-cancel"
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(null)}
                  >
                    取消
                  </button>
                  <button
                    type="button"
                    data-testid="provider-delete-confirm-button"
                    disabled={busy}
                    onClick={() => void onConfirmDelete(provider.id)}
                  >
                    删除
                  </button>
                </div>
              ) : (
                <div className="provider-actions">
                  <button
                    type="button"
                    data-testid="provider-edit"
                    data-provider-id={provider.id}
                    disabled={busy}
                    onClick={() => openEdit(provider)}
                  >
                    编辑
                  </button>
                  <button
                    type="button"
                    data-testid="provider-delete"
                    data-provider-id={provider.id}
                    data-is-builtin={String(provider.is_builtin)}
                    disabled={busy || provider.is_builtin}
                    title={provider.is_builtin ? "内置供应商不可删除" : "删除供应商"}
                    onClick={() => setConfirmDeleteId(provider.id)}
                  >
                    删除
                  </button>
                  <label className="provider-toggle-label">
                    <input
                      type="checkbox"
                      data-testid="provider-toggle"
                      data-provider-id={provider.id}
                      data-enabled={String(provider.enabled)}
                      checked={provider.enabled}
                      disabled={busy}
                      onChange={(event) => void onToggleProvider(provider.id, event.target.checked)}
                    />
                    启用
                  </label>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}

      {notice ? (
        <p className="provider-notice" data-testid="providers-notice" role="status">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          className="provider-error"
          data-testid="provider-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {errorCode === "builtin_provider_undeletable" ? "内置供应商不可删除" : error}
        </p>
      ) : null}
    </section>
  );
}
