/**
 * 输入区模型选择器（M3-11；ADR-010 决策 3；UI-UX §2.4/§7.3 + M3-11 前置登记）。
 *
 * - 从**启用供应商的启用模型**派生（分组 + 搜索）；作用于新建会话
 *   （`session.create.model`，UI-05 既有透传路径）；
 * - 与新建表单手动输入共存（M3-11 前置登记：选择结果写入同一草稿字段）；
 * - 空态：无可派生模型时展示原型文案（不阻断手动输入）；
 * - 失效回退：已选模型随供应商/模型停用消失时回退到首个可用模型（无则清空）；
 * - 运行期无切换入口（无 update 命令；会话级模型只读展示）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  deriveModelGroups,
  ensureSelectedModelValid,
  findModelProvider,
  providerAbbr,
  providerColor,
  providersIpc as productionProvidersIpc,
  type ModelGroup,
  type ProviderEntry,
  type ProvidersIpc,
} from "./providers";
import { describeIpcError } from "./startup";

export interface ModelSelectorProps {
  /** 供应商 IPC（测试/E2E 注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: ProvidersIpc;
  /** 当前选择（作用于新建会话；`null` = 未选择）。 */
  value: string | null;
  /** 选择回调（`null` = 清空/回退到无选择）。 */
  onChange: (modelId: string | null) => void;
  /** 激活会话的会话级模型（只读展示；不提供修改入口）。 */
  sessionModel?: string | null;
  disabled?: boolean;
}

export function ModelSelector({
  ipc = productionProvidersIpc,
  value,
  onChange,
  sessionModel = null,
  disabled = false,
}: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const [providers, setProviders] = useState<ProviderEntry[]>([]);
  const [search, setSearch] = useState("");
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await ipc.list();
      setProviders(list);
      setError(null);
      return list;
    } catch (failure) {
      setProviders([]);
      setError(describeIpcError(failure));
      return [];
    }
  }, [ipc]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const groups = useMemo(() => deriveModelGroups(providers), [providers]);

  // 选中失效回退（UI-UX §2.4 M3-11 前置登记：已选择但已失效 → 首个可用模型；
  // 此前未选择 → 保持空选择，不自动为新建会话预选模型）。
  useEffect(() => {
    if (value === null) {
      return;
    }
    const selection = findModelProvider(providers, value);
    const next = ensureSelectedModelValid(
      selection ? { providerId: selection.id, modelId: value } : null,
      groups,
    );
    if (next === null) {
      onChange(null);
    } else if (next.modelId !== value) {
      onChange(next.modelId);
    }
  }, [groups, onChange, providers, value]);

  const toggle = useCallback(async () => {
    const next = !open;
    setOpen(next);
    if (next) {
      setSearch("");
      await refresh();
    }
  }, [open, refresh]);

  const selectedProvider = value ? findModelProvider(providers, value) : null;
  const selectedModel = selectedProvider?.models.find((model) => model.model_id === value);
  const label = selectedModel
    ? `${selectedProvider?.name ?? ""} · ${selectedModel.display_name}`
    : value !== null
      ? value
      : sessionModel
        ? `会话模型：${sessionModel}（只读）`
        : "选择模型";

  const keyword = search.trim().toLowerCase();
  const filtered: ModelGroup[] = groups
    .map((group) => ({
      ...group,
      models: group.models.filter(
        (model) =>
          keyword.length === 0 ||
          model.display_name.toLowerCase().includes(keyword) ||
          model.model_id.toLowerCase().includes(keyword) ||
          group.name.toLowerCase().includes(keyword),
      ),
    }))
    .filter((group) => group.models.length > 0);

  return (
    <div className="model-selector" data-testid="model-selector" data-value={value ?? ""}>
      <button
        type="button"
        className={value ? "model-selector-btn" : "model-selector-btn empty"}
        data-testid="model-selector-toggle"
        aria-expanded={open}
        disabled={disabled}
        title="模型选择（作用于新建会话；运行期只读）"
        onClick={() => void toggle()}
      >
        {selectedProvider ? (
          <span
            className="model-selector-icon"
            style={{ background: providerColor(selectedProvider.type) }}
          >
            {providerAbbr(selectedProvider)}
          </span>
        ) : null}
        <span className="model-selector-label">{label}</span>
      </button>

      {open ? (
        <div className="model-selector-list" data-testid="model-selector-list">
          <input
            type="search"
            className="model-selector-search"
            data-testid="model-selector-search"
            placeholder="搜索模型"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
          {error ? <p className="model-selector-error">{error}</p> : null}
          {filtered.length === 0 ? (
            <p className="model-selector-empty" data-testid="model-selector-empty">
              {keyword.length > 0
                ? "没有匹配的模型"
                : "没有可用的模型；请先在「设置 → 模型与供应商配置」中启用供应商和模型（也可在新建表单手动输入模型 ID）"}
            </p>
          ) : (
            filtered.map((group) => (
              <div key={group.id} className="model-selector-group">
                <div className="model-selector-group-label">
                  <span
                    className="model-selector-icon"
                    style={{ background: providerColor(groupProviderType(group, providers)) }}
                  >
                    {group.name.trim().charAt(0).toUpperCase()}
                  </span>
                  <span>{group.name}</span>
                </div>
                {group.models.map((model) => {
                  const selected = model.model_id === value;
                  return (
                    <button
                      key={model.id}
                      type="button"
                      className={selected ? "model-selector-item on" : "model-selector-item"}
                      data-testid="model-selector-item"
                      data-provider-id={group.id}
                      data-model-id={model.model_id}
                      data-enabled={String(model.enabled)}
                      onClick={() => {
                        onChange(model.model_id);
                        setOpen(false);
                      }}
                    >
                      <span className="model-selector-item-name">{model.display_name}</span>
                      <span className="model-selector-item-id">{model.model_id}</span>
                      {selected ? <span className="model-selector-check">✓</span> : null}
                    </button>
                  );
                })}
              </div>
            ))
          )}
        </div>
      ) : null}
    </div>
  );
}

function groupProviderType(group: ModelGroup, providers: ProviderEntry[]) {
  return providers.find((provider) => provider.id === group.id)?.type ?? "custom";
}
