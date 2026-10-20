/**
 * Agent 预设管理页（M7-09；ADR-026 §2.2；UI-UX §7.3「预设」锚点）。
 *
 * - 列表（`presets_list`）：每条预设锚点 `preset-item` + `data-preset-id` / `data-builtin`；
 *   内置预设带「内置」标记（**可编辑、不可删除**——删除按钮置灰，与服务端硬约束一致）；
 * - 使用中标记：`data-active`（新建会话入口当前选中的预设；原型「使用中」标签）；
 * - 编辑（`preset-edit` 容器 + 名称/提示词表单 → `preset_update`；保存后对新会话生效）；
 * - 新增（`preset_add`，自定义预设 `is_builtin=0`）/ 删除（`preset_delete` + 二次确认）；
 * - 错误呈现沿用既有模式（错误元素 `data-code`，UI-UX §7.3 通用行）：
 *   `preset_not_found` → 「预设不存在，已刷新列表」且自动重取；
 *   `builtin_preset_undeletable` → 「内置预设不可删除」。
 */
import { useCallback, useEffect, useState } from "react";

import {
  BUILTIN_PRESET_UNDELETABLE_CODE,
  BUILTIN_PRESET_UNDELETABLE_MESSAGE,
  PRESET_NOT_FOUND_CODE,
  PRESET_NOT_FOUND_MESSAGE,
  presetNameTooLong,
  presetPromptTooLarge,
  presetsIpc as productionPresetsIpc,
  type PresetEntry,
  type PresetsIpc,
} from "./presets";
import { describeIpcError, ipcErrorCode } from "./startup";

export interface PresetsPageProps {
  /** 预设命令面（测试/E2E 注入替身；缺省 = 生产 Tauri 实现）。 */
  ipc?: PresetsIpc;
  /** 新建会话入口当前选中的预设（「使用中」标记数据源；`null` = 未选）。 */
  activePresetId?: string | null;
  /** 选中/取消选中预设（新建会话入口同步；缺省 = 只读展示）。 */
  onActivePresetChange?: (presetId: string | null) => void;
  /** 预设增改删成功后回调（新建会话入口的预设清单需要刷新；缺省 = 不通知）。 */
  onChanged?: () => void;
}

/** 提示词预览（列表行；超出截断，不展示完整长文）。 */
function promptPreview(systemPrompt: string): string {
  const flat = systemPrompt.replace(/\s+/g, " ").trim();
  return flat.length > 60 ? `${flat.slice(0, 60)}…` : flat;
}

export function PresetsPage({
  ipc = productionPresetsIpc,
  activePresetId = null,
  onActivePresetChange,
  onChanged,
}: PresetsPageProps) {
  const [presets, setPresets] = useState<PresetEntry[] | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editName, setEditName] = useState("");
  const [editPrompt, setEditPrompt] = useState("");
  const [newName, setNewName] = useState("");
  const [newPrompt, setNewPrompt] = useState("");
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorCode, setErrorCode] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const reportError = useCallback((failure: unknown) => {
    setError(describeIpcError(failure));
    setErrorCode(ipcErrorCode(failure));
  }, []);
  const clearError = useCallback(() => {
    setError(null);
    setErrorCode(null);
  }, []);

  /** 刷新清单（`preset_not_found` 路径复用：自动重取）。 */
  const refresh = useCallback(async () => {
    try {
      const list = await ipc.listPresets();
      setPresets(list);
      clearError();
    } catch (failure) {
      setPresets([]);
      reportError(failure);
    }
  }, [clearError, ipc, reportError]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** 错误收口：两个登记码走 UI-UX §5 文案（前者附带自动刷新）。 */
  const handlePresetError = useCallback(
    async (failure: unknown) => {
      const code = ipcErrorCode(failure);
      if (code === PRESET_NOT_FOUND_CODE) {
        setError(PRESET_NOT_FOUND_MESSAGE);
        setErrorCode(PRESET_NOT_FOUND_CODE);
        await refresh();
        return;
      }
      if (code === BUILTIN_PRESET_UNDELETABLE_CODE) {
        setError(BUILTIN_PRESET_UNDELETABLE_MESSAGE);
        setErrorCode(BUILTIN_PRESET_UNDELETABLE_CODE);
        await refresh();
        return;
      }
      reportError(failure);
    },
    [refresh, reportError],
  );

  const onStartEdit = useCallback((preset: PresetEntry) => {
    setEditingId(preset.id);
    setEditName(preset.name);
    setEditPrompt(preset.system_prompt);
    setConfirmDeleteId(null);
  }, []);

  const onSaveEdit = useCallback(async () => {
    if (editingId === null) {
      return;
    }
    const name = editName.trim();
    if (!name) {
      setError("请填写预设名称");
      setErrorCode("missing_field");
      return;
    }
    if (presetNameTooLong(name)) {
      setError("预设名称最长 64 字符");
      setErrorCode("too_large");
      return;
    }
    if (presetPromptTooLarge(editPrompt)) {
      setError("系统提示词最大 32KiB");
      setErrorCode("too_large");
      return;
    }
    setBusy(true);
    clearError();
    setNotice(null);
    try {
      await ipc.updatePreset({ preset_id: editingId, name, system_prompt: editPrompt });
      setEditingId(null);
      setNotice("预设已保存（对新会话生效）");
      onChanged?.();
      await refresh();
    } catch (failure) {
      await handlePresetError(failure);
    } finally {
      setBusy(false);
    }
  }, [clearError, editName, editPrompt, editingId, handlePresetError, ipc, onChanged, refresh]);

  const onAdd = useCallback(async () => {
    const name = newName.trim();
    if (!name) {
      setError("请填写预设名称");
      setErrorCode("missing_field");
      return;
    }
    if (presetNameTooLong(name)) {
      setError("预设名称最长 64 字符");
      setErrorCode("too_large");
      return;
    }
    if (presetPromptTooLarge(newPrompt)) {
      setError("系统提示词最大 32KiB");
      setErrorCode("too_large");
      return;
    }
    setBusy(true);
    clearError();
    setNotice(null);
    try {
      const created = await ipc.addPreset({ name, system_prompt: newPrompt });
      setNewName("");
      setNewPrompt("");
      setNotice(`已新增预设「${created.name}」（新建会话可选）`);
      onChanged?.();
      await refresh();
    } catch (failure) {
      await handlePresetError(failure);
    } finally {
      setBusy(false);
    }
  }, [clearError, handlePresetError, ipc, newName, newPrompt, onChanged, refresh]);

  const onConfirmDelete = useCallback(
    async (presetId: string) => {
      setBusy(true);
      clearError();
      setNotice(null);
      try {
        await ipc.deletePreset(presetId);
        setConfirmDeleteId(null);
        if (activePresetId === presetId) {
          onActivePresetChange?.(null);
        }
        setNotice("预设已删除（既有会话提示词保持原样）");
        onChanged?.();
        await refresh();
      } catch (failure) {
        setConfirmDeleteId(null);
        await handlePresetError(failure);
      } finally {
        setBusy(false);
      }
    },
    [
      activePresetId,
      clearError,
      handlePresetError,
      ipc,
      onActivePresetChange,
      onChanged,
      refresh,
    ],
  );

  return (
    <section className="presets-page" data-testid="presets-page">
      <h3>Agent 预设</h3>
      <p className="settings-note">
        每个预设对应一套系统提示词；与工作区记忆注入按「预设块 → 工作区约定块」合成
        （互不覆盖）。保存后对新会话生效，既有会话不受影响。
      </p>

      {presets === null ? (
        <p data-testid="presets-loading">正在读取预设…</p>
      ) : (
        <ul className="preset-list" data-testid="preset-list">
          {presets.map((preset) => (
            <li
              key={preset.id}
              className="preset-item"
              data-testid="preset-item"
              data-preset-id={preset.id}
              data-builtin={String(preset.is_builtin)}
              data-active={String(preset.id === activePresetId)}
            >
              <div className="preset-item-main">
                <span className="preset-name" data-testid="preset-name">
                  {preset.name}
                </span>
                {preset.is_builtin ? (
                  <span className="preset-builtin" data-testid="preset-builtin">
                    内置
                  </span>
                ) : null}
                {preset.id === activePresetId ? (
                  <span className="preset-in-use" data-testid="preset-in-use">
                    使用中
                  </span>
                ) : null}
                <span className="preset-preview" data-testid="preset-preview">
                  {promptPreview(preset.system_prompt)}
                </span>
              </div>
              <div className="preset-actions">
                {onActivePresetChange ? (
                  <button
                    type="button"
                    data-testid="preset-use"
                    data-preset-id={preset.id}
                    disabled={busy}
                    onClick={() =>
                      onActivePresetChange(preset.id === activePresetId ? null : preset.id)
                    }
                  >
                    {preset.id === activePresetId ? "取消使用" : "使用"}
                  </button>
                ) : null}
                <button
                  type="button"
                  data-testid="preset-edit"
                  data-preset-id={preset.id}
                  disabled={busy}
                  onClick={() => onStartEdit(preset)}
                >
                  编辑
                </button>
                <button
                  type="button"
                  data-testid="preset-delete"
                  data-preset-id={preset.id}
                  // 内置预设删除入口置灰（服务端同口径硬拒绝；不以 UI 隐藏代替）。
                  disabled={busy || preset.is_builtin}
                  title={preset.is_builtin ? BUILTIN_PRESET_UNDELETABLE_MESSAGE : undefined}
                  onClick={() => setConfirmDeleteId(preset.id)}
                >
                  删除
                </button>
              </div>
              {confirmDeleteId === preset.id ? (
                <div className="preset-delete-confirm" data-testid="preset-delete-confirm">
                  <span>删除后不可恢复；既有会话的系统提示词保持原样。</span>
                  <button
                    type="button"
                    data-testid="preset-delete-confirm-button"
                    disabled={busy}
                    onClick={() => void onConfirmDelete(preset.id)}
                  >
                    确认删除
                  </button>
                  <button
                    type="button"
                    data-testid="preset-delete-cancel"
                    disabled={busy}
                    onClick={() => setConfirmDeleteId(null)}
                  >
                    取消
                  </button>
                </div>
              ) : null}
              {editingId === preset.id ? (
                <div className="preset-edit" data-testid="preset-edit-form" data-preset-id={preset.id}>
                  <label className="preset-edit-row">
                    名称
                    <input
                      type="text"
                      data-testid="preset-edit-name"
                      value={editName}
                      maxLength={64}
                      disabled={busy}
                      onChange={(event) => setEditName(event.target.value)}
                    />
                  </label>
                  <label className="preset-edit-row">
                    系统提示词
                    <textarea
                      data-testid="preset-edit-prompt"
                      value={editPrompt}
                      rows={6}
                      disabled={busy}
                      onChange={(event) => setEditPrompt(event.target.value)}
                    />
                  </label>
                  <div className="preset-actions">
                    <button
                      type="button"
                      data-testid="preset-edit-save"
                      disabled={busy}
                      onClick={() => void onSaveEdit()}
                    >
                      保存
                    </button>
                    <button
                      type="button"
                      data-testid="preset-edit-cancel"
                      disabled={busy}
                      onClick={() => setEditingId(null)}
                    >
                      取消
                    </button>
                  </div>
                </div>
              ) : null}
            </li>
          ))}
          {presets.length === 0 ? (
            <li className="preset-empty" data-testid="preset-empty">
              暂无预设
            </li>
          ) : null}
        </ul>
      )}

      <section className="preset-add" data-testid="preset-add">
        <h4>新增预设</h4>
        <label className="preset-edit-row">
          名称（≤64 字符）
          <input
            type="text"
            data-testid="preset-add-name"
            value={newName}
            maxLength={64}
            disabled={busy}
            onChange={(event) => setNewName(event.target.value)}
          />
        </label>
        <label className="preset-edit-row">
          系统提示词（≤32KiB）
          <textarea
            data-testid="preset-add-prompt"
            value={newPrompt}
            rows={4}
            disabled={busy}
            onChange={(event) => setNewPrompt(event.target.value)}
          />
        </label>
        <div className="preset-actions">
          <button
            type="button"
            data-testid="preset-add-submit"
            disabled={busy}
            onClick={() => void onAdd()}
          >
            新增预设
          </button>
        </div>
      </section>

      {notice ? (
        <p className="settings-notice" data-testid="presets-notice">
          {notice}
        </p>
      ) : null}

      {error ? (
        <p
          className="settings-error"
          data-testid="presets-error"
          data-code={errorCode ?? ""}
          role="alert"
        >
          {error}
        </p>
      ) : null}
    </section>
  );
}
