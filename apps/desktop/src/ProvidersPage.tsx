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
 * - M7-06/ADR-024：`测试连接` = 真实调用（`provider_test`；三态 + 超时；锚点
 *   `provider-test-result`，`data-status` = ok / unreachable / auth_failed / timeout）；
 *   `从供应商获取` = 真实调用（`provider_models_fetch`；归一化 + 幂等 upsert）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  OFFICIAL_PROVIDER_PRESETS,
  PROVIDER_ENDPOINT_PRESETS,
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

/** 连通性测试展示态（UI-UX §7.3 `provider-test-result` 的 `data-status` 取值域）。 */
export type TestStatus = "ok" | "unreachable" | "auth_failed" | "timeout";

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
  /** ADR-028：协议端点（`anthropic`/`openai`/`google`/`dsh`）。 */
  const [formEndpoints, setFormEndpoints] = useState<Record<string, string>>({});
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
  /** M7-06/ADR-024：连通性测试三态/超时回执（锚点 provider-test-result）。 */
  const [testResult, setTestResult] = useState<{ status: TestStatus; text: string } | null>(null);
  /** M7-06/ADR-024：模型获取回执（锚点 provider-models-notice）。 */
  const [modelsNotice, setModelsNotice] = useState<string | null>(null);

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
    setTestResult(null);
    setModelsNotice(null);
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
      setFormEndpoints({ ...PROVIDER_ENDPOINT_PRESETS.anthropic.endpoints });
    } else {
      setFormType("custom");
      setFormName("");
      setFormBaseUrl("");
      setFormEndpoints({});
    }
    setView("form");
  }, []);

  const openEdit = useCallback((provider: ProviderEntry) => {
    setEditing(provider);
    setFormMode(provider.type === "custom" ? "custom" : "official");
    setFormType(provider.type);
    setFormName(provider.name);
    setFormBaseUrl(provider.base_url ?? "");
    const endpointPreset = PROVIDER_ENDPOINT_PRESETS[provider.type]?.endpoints ?? {};
    setFormEndpoints(
      provider.endpoints && Object.keys(provider.endpoints).length > 0
        ? { ...provider.endpoints }
        : { ...endpointPreset },
    );
    setFormApiKey("");
    setApiKeyTouched(false);
    setShowKey(false);
    setFormEnabled(provider.enabled);
    setModelIdDraft("");
    setModelNameDraft("");
    setConfirmDeleteId(null);
    setNotice(null);
    setTestResult(null);
    setModelsNotice(null);
    setError(null);
    setErrorCode(null);
    setView("form");
  }, []);

  const backToList = useCallback(() => {
    setView("list");
    setEditing(null);
    setConfirmDeleteId(null);
    setTestResult(null);
    setModelsNotice(null);
    clearError();
    void refresh();
  }, [clearError, refresh]);

  const onSelectOfficialType = useCallback((type: ProviderType) => {
    setFormType(type);
    if (type !== "custom") {
      const preset = OFFICIAL_PROVIDER_PRESETS[type];
      setFormName(preset.name);
      setFormBaseUrl(preset.baseUrl);
      setFormEndpoints({ ...PROVIDER_ENDPOINT_PRESETS[type].endpoints });
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
    setTestResult(null);
    setModelsNotice(null);
    const endpoints = Object.fromEntries(
      Object.entries(formEndpoints)
        .map(([protocol, url]) => [protocol, url.trim()] as const)
        .filter(([, url]) => url.length > 0),
    );
    try {
      if (editing) {
        const input: Parameters<ProvidersIpc["update"]>[0] = {
          id: editing.id,
          name,
          base_url: baseUrl,
          endpoints,
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
          endpoints,
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
    formEndpoints,
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

  /** M7-06/ADR-024：真实连通性测试（成功/不可达/凭据无效/超时；其余走错误块）。 */
  const onTest = useCallback(async () => {
    if (!editing) {
      return;
    }
    setBusy(true);
    clearError();
    setTestResult(null);
    try {
      const result = await ipc.test(editing.id);
      setTestResult({
        status: "ok",
        text: `连通性测试通过（${result.latency_ms}ms，${result.models_count} 个模型）`,
      });
    } catch (failure) {
      const code = ipcErrorCode(failure);
      const message = describeIpcError(failure);
      if (code === "provider_auth_failed") {
        setTestResult({ status: "auth_failed", text: message });
      } else if (code === "provider_unreachable") {
        setTestResult({
          status: message.includes("超时") ? "timeout" : "unreachable",
          text: message,
        });
      } else {
        reportError(failure);
      }
    } finally {
      setBusy(false);
    }
  }, [clearError, editing, ipc, reportError]);

  /** M7-06/ADR-024：从供应商获取模型（归一化 + 幂等 upsert；刷新表单模型列表）。 */
  const onFetchModels = useCallback(async () => {
    if (!editing) {
      return;
    }
    setBusy(true);
    clearError();
    setModelsNotice(null);
    try {
      const result = await ipc.fetchModels(editing.id);
      setModelsNotice(
        `已获取 ${result.fetched} 个模型（新增 ${result.added} / 刷新 ${result.refreshed}）`,
      );
      await refresh(editing.id);
    } catch (failure) {
      reportError(failure);
    } finally {
      setBusy(false);
    }
  }, [clearError, editing, ipc, refresh, reportError]);

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
          <div className="form-head">
            <button
              type="button"
              className="back"
              data-testid="provider-form-back"
              aria-label="返回供应商列表"
              onClick={backToList}
            >
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M19 12H5M12 19l-7-7 7-7" />
              </svg>
            </button>
            <div className="title">{formTitle}</div>
            <button
              type="button"
              className="btn-primary"
              data-testid="provider-form-save"
              disabled={busy}
              onClick={() => void onSave()}
            >
              {editing ? "保存" : "创建"}
            </button>
          </div>

          <div className="form-section-title">基本信息</div>
          <div className="form-card">
            {formMode === "official" && !editing ? (
              <div className="form-row">
                <div className="form-label">供应商类型</div>
                <select
                  className="form-input"
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
              </div>
            ) : null}

            <div className="form-row">
              <div className="form-label">供应商名称</div>
              <input
                type="text"
                className="form-input"
                data-testid="provider-name-input"
                placeholder="例如: My Anthropic"
                value={formName}
                disabled={busy}
                onChange={(event) => setFormName(event.target.value)}
              />
            </div>

            <div className="form-row">
              <div className="form-label">Base URL</div>
              <div className="preview-hint">
                预览：<b>{previewPath}</b>
              </div>
              <input
                type="text"
                className="form-input"
                data-testid="provider-base-url-input"
                placeholder="https://api.example.com"
                value={formBaseUrl}
                disabled={busy}
                onChange={(event) => setFormBaseUrl(event.target.value)}
              />
            </div>

            <div className="form-row">
              <div className="form-label">协议端点（默认已按供应商预填，可覆盖）</div>
              <div className="provider-endpoint-grid">
                <label className="provider-endpoint-field">
                  <span>Anthropic 兼容（Claude Code）</span>
                  <input
                    type="text"
                    className="form-input"
                    data-testid="provider-endpoint-anthropic"
                    placeholder="https://api.example.com/anthropic"
                    value={formEndpoints.anthropic ?? ""}
                    disabled={busy}
                    onChange={(event) =>
                      setFormEndpoints((current) => ({
                        ...current,
                        anthropic: event.target.value,
                      }))
                    }
                  />
                </label>
                <label className="provider-endpoint-field">
                  <span>OpenAI 兼容（Codex）</span>
                  <input
                    type="text"
                    className="form-input"
                    data-testid="provider-endpoint-openai"
                    placeholder="https://api.example.com/v1"
                    value={formEndpoints.openai ?? ""}
                    disabled={busy}
                    onChange={(event) =>
                      setFormEndpoints((current) => ({
                        ...current,
                        openai: event.target.value,
                      }))
                    }
                  />
                </label>
                <label className="provider-endpoint-field">
                  <span>Google / 原生</span>
                  <input
                    type="text"
                    className="form-input"
                    data-testid="provider-endpoint-google"
                    placeholder="https://generativelanguage.googleapis.com"
                    value={formEndpoints.google ?? ""}
                    disabled={busy}
                    onChange={(event) =>
                      setFormEndpoints((current) => ({
                        ...current,
                        google: event.target.value,
                      }))
                    }
                  />
                </label>
                <label className="provider-endpoint-field">
                  <span>DSH / 原生</span>
                  <input
                    type="text"
                    className="form-input"
                    data-testid="provider-endpoint-dsh"
                    placeholder="DSH profile 端点（可选）"
                    value={formEndpoints.dsh ?? ""}
                    disabled={busy}
                    onChange={(event) =>
                      setFormEndpoints((current) => ({
                        ...current,
                        dsh: event.target.value,
                      }))
                    }
                  />
                </label>
              </div>
              <p className="preview-hint">
                Claude Code 使用 Anthropic 端点；Codex 使用 OpenAI 端点；端点不是密钥，可随时修改。
              </p>
            </div>

            <div className="form-row">
              <div className="form-label">
                <span>API Key</span>
                <button
                  type="button"
                  className="btn-test"
                  data-testid="provider-test"
                  disabled={busy || !editing}
                  title={editing ? "发起真实连通性测试（10s 超时）" : "创建保存后可测试连接"}
                  onClick={() => void onTest()}
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M13 2L3 14h7l-1 8 10-12h-7l1-8z" />
                  </svg>
                  测试连接
                </button>
              </div>
              {editing?.api_key_ref ? (
                <p className="provider-key-ref" data-testid="provider-api-key-ref-readonly">
                  当前密钥引用（只读）：{editing.api_key_ref}
                </p>
              ) : null}
              <div className="input-wrap provider-key-input">
                <input
                  className="form-input"
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
                  className="eye"
                  data-testid="provider-api-key-reveal"
                  title={showKey ? "隐藏密钥" : "显示密钥"}
                  aria-label={showKey ? "隐藏密钥" : "显示密钥"}
                  disabled={busy}
                  onClick={() => setShowKey((visible) => !visible)}
                >
                  {showKey ? (
                    <svg viewBox="0 0 24 24" aria-hidden="true">
                      <path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z" />
                      <circle cx="12" cy="12" r="3" />
                      <path d="M3 3l18 18" />
                    </svg>
                  ) : (
                    <svg viewBox="0 0 24 24" aria-hidden="true">
                      <path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z" />
                      <circle cx="12" cy="12" r="3" />
                    </svg>
                  )}
                </button>
              </div>
              {editing && apiKeyTouched && formApiKey.length === 0 && editing.api_key_ref ? (
                <p className="provider-key-hint">保存后将清除密钥引用（keychain 条目一并删除）</p>
              ) : null}
            </div>

            <div className="form-row">
              <div className="switch-row">
                <div className="switch-label">
                  <div className="n">启用此配置</div>
                  <div className="d">关闭后该配置的模型不会出现在选择列表中</div>
                </div>
                <label className="switch">
                  <input
                    type="checkbox"
                    data-testid="provider-enabled-switch"
                    checked={formEnabled}
                    disabled={busy}
                    onChange={(event) => setFormEnabled(event.target.checked)}
                  />
                  <span />
                </label>
              </div>
            </div>
          </div>

          {editing ? (
            <section className="provider-models">
              <div className="form-section-title">已启用模型</div>
              {editing.models.filter((model) => model.enabled).length === 0 ? (
                <p className="provider-model-empty empty-state">
                  还没有启用任何模型，从下方可用模型中选择
                </p>
              ) : (
                <ul className="provider-model-list mlist">
                  {editing.models
                    .filter((model) => model.enabled)
                    .map((model) => (
                      <li
                        key={model.id}
                        className="provider-model-item mlist-item"
                        data-testid="provider-model-item"
                        data-model-id={model.model_id}
                        data-enabled={String(model.enabled)}
                      >
                        <span
                          className="provider-model-icon mi"
                          style={{ background: providerColor(editing.type) }}
                        >
                          {providerAbbr(editing)}
                        </span>
                        <div className="provider-model-info info">
                          <span className="provider-model-name n">{model.display_name}</span>
                          <span className="provider-model-id mid">{model.model_id}</span>
                        </div>
                        <button
                          type="button"
                          className="act on"
                          data-testid="provider-model-toggle"
                          data-model-id={model.model_id}
                          data-action="disable"
                          disabled={busy}
                          onClick={() => void onToggleModel(model, false)}
                        >
                          停用
                        </button>
                      </li>
                    ))}
                </ul>
              )}

              <div className="models-head">
                <div className="form-section-title" style={{ marginBottom: 0 }}>
                  可用模型
                </div>
                <button
                  type="button"
                  className="btn-fetch"
                  data-testid="provider-models-fetch"
                  disabled={busy}
                  title="从供应商获取模型列表（归一化 + 幂等写入）"
                  onClick={() => void onFetchModels()}
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M12 3v12M7 10l5 5 5-5M5 21h14" />
                  </svg>
                  从供应商获取
                </button>
              </div>
              {editing.models.filter((model) => !model.enabled).length === 0 ? (
                <p className="provider-model-empty empty-state">没有未启用的模型</p>
              ) : (
                <ul className="provider-model-list mlist">
                  {editing.models
                    .filter((model) => !model.enabled)
                    .map((model) => (
                      <li
                        key={model.id}
                        className="provider-model-item mlist-item"
                        data-testid="provider-model-item"
                        data-model-id={model.model_id}
                        data-enabled={String(model.enabled)}
                      >
                        <span
                          className="provider-model-icon mi"
                          style={{ background: providerColor(editing.type), opacity: 0.5 }}
                        >
                          {providerAbbr(editing)}
                        </span>
                        <div className="provider-model-info info">
                          <span className="provider-model-name n">{model.display_name}</span>
                          <span className="provider-model-id mid">{model.model_id}</span>
                        </div>
                        <button
                          type="button"
                          className="act off"
                          data-testid="provider-model-toggle"
                          data-model-id={model.model_id}
                          data-action="enable"
                          disabled={busy}
                          onClick={() => void onToggleModel(model, true)}
                        >
                          启用
                        </button>
                      </li>
                    ))}
                </ul>
              )}

              <div className="model-input-row provider-model-add-row">
                <input
                  type="text"
                  className="form-input"
                  data-testid="provider-model-id-input"
                  placeholder="模型 ID（如 claude-opus-4-6）"
                  value={modelIdDraft}
                  disabled={busy}
                  onChange={(event) => setModelIdDraft(event.target.value)}
                />
                <input
                  type="text"
                  className="form-input"
                  data-testid="provider-model-name-input"
                  placeholder="显示名称（可选）"
                  value={modelNameDraft}
                  disabled={busy}
                  onChange={(event) => setModelNameDraft(event.target.value)}
                />
                <button
                  type="button"
                  className="btn-plus"
                  data-testid="provider-model-add"
                  disabled={busy}
                  title="添加模型"
                  aria-label="添加模型"
                  onClick={() => void onAddModel()}
                >
                  ＋
                </button>
              </div>
              {modelsNotice ? (
                <p className="provider-notice" data-testid="provider-models-notice" role="status">
                  {modelsNotice}
                </p>
              ) : null}
            </section>
          ) : (
            <p className="provider-models-hint">创建保存后可管理模型</p>
          )}

          {testResult ? (
            <p
              className="provider-notice"
              data-testid="provider-test-result"
              data-status={testResult.status}
              role="status"
            >
              {testResult.text}
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
      <div className="mc-head">
        <div>
          <div className="mc-ttl">模型配置</div>
          <div className="mc-desc">
            管理 AI 供应商连接，配置 API Key 和可用模型。启用的供应商与模型会出现在会话输入框的模型选择器中。
          </div>
        </div>
        <div className="mc-actions">
          <button
            type="button"
            className="btn-add"
            data-testid="providers-add"
            disabled={busy}
            onClick={() => openCreate("official")}
          >
            ＋ 添加供应商
          </button>
          <button
            type="button"
            className="btn-add"
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
        <p className="providers-empty empty-state" data-testid="provider-empty">
          还没有供应商配置，点击右上角添加
        </p>
      ) : (
        <ul className="provider-cards prov-cards">
          {providers.map((provider) => (
            <li
              key={provider.id}
              className="provider-card prov-card"
              data-testid="provider-card"
              data-provider-id={provider.id}
              data-enabled={String(provider.enabled)}
              data-is-builtin={String(provider.is_builtin)}
            >
              <span
                className="provider-icon prov-icon"
                style={{ background: providerColor(provider.type) }}
              >
                {providerAbbr(provider)}
              </span>
              <div className="provider-info prov-info">
                <div className="provider-name prov-name">{provider.name}</div>
                <div className="provider-desc prov-desc">{providerDesc(provider)}</div>
              </div>
              {confirmDeleteId === provider.id ? (
                <div className="provider-confirm prov-confirm" data-testid="provider-delete-confirm">
                  <span className="provider-confirm-text warn">确认删除？</span>
                  <button
                    type="button"
                    className="btn-sm cancel"
                    data-testid="provider-delete-cancel"
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(null)}
                  >
                    取消
                  </button>
                  <button
                    type="button"
                    className="btn-sm danger"
                    data-testid="provider-delete-confirm-button"
                    disabled={busy}
                    onClick={() => void onConfirmDelete(provider.id)}
                  >
                    删除
                  </button>
                </div>
              ) : (
                <div className="provider-actions prov-actions">
                  <div className="provider-tools prov-tools">
                    <button
                      type="button"
                      className="icon-btn"
                      data-testid="provider-edit"
                      data-provider-id={provider.id}
                      disabled={busy}
                      title="编辑"
                      aria-label={`编辑 ${provider.name}`}
                      onClick={() => openEdit(provider)}
                    >
                      <svg viewBox="0 0 24 24" aria-hidden="true">
                        <path d="M12 20h9" />
                        <path d="M16.5 3.5a2.12 2.12 0 013 3L7 19l-4 1 1-4z" />
                      </svg>
                    </button>
                    <button
                      type="button"
                      className="icon-btn danger"
                      data-testid="provider-delete"
                      data-provider-id={provider.id}
                      data-is-builtin={String(provider.is_builtin)}
                      disabled={busy || provider.is_builtin}
                      title={provider.is_builtin ? "内置供应商不可删除" : "删除供应商"}
                      aria-label={`删除 ${provider.name}`}
                      onClick={() => setConfirmDeleteId(provider.id)}
                    >
                      <svg viewBox="0 0 24 24" aria-hidden="true">
                        <path d="M3 6h18M8 6V4a1 1 0 011-1h6a1 1 0 011 1v2M19 6l-1 14a2 2 0 01-2 2H8a2 2 0 01-2-2L5 6" />
                        <path d="M10 11v6M14 11v6" />
                      </svg>
                    </button>
                  </div>
                  <label className="switch provider-toggle-label">
                    <input
                      type="checkbox"
                      data-testid="provider-toggle"
                      data-provider-id={provider.id}
                      data-enabled={String(provider.enabled)}
                      checked={provider.enabled}
                      disabled={busy}
                      aria-label={`启用 ${provider.name}`}
                      onChange={(event) => void onToggleProvider(provider.id, event.target.checked)}
                    />
                    <span />
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
