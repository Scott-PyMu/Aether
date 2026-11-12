/**
 * 会话级权限模式选择器（M6-01；ADR-014 决策 3 / M6-01 DoD8；UI-UX §2.4）。
 *
 * - 三档（与运行时同名同序）：`read_only` 仅查看 / `workspace_write` 工作区内修改
 *   （缺省）/ `danger_full_access` 完全权限；点击选项调用 `onChange(mode)`
 *   （真实 IPC 由父组件 SessionWorkbench 回调用，失败走既有错误呈现）；
 * - 无激活会话（`sessionId === null`）或 `disabled` 时禁用 + 空态（「无会话」）；
 * - 弹层内**常驻一行**边界声明（`permission-mode-boundary`，R14：不随档位切换
 *   消失/清空；文案为 ADR-014 附录 F-3 定稿）——弹层内容始终渲染、`hidden` /
 *   `data-open` 控制显隐，保证断言稳定；
 * - 文案纪律（ADR-014 附录 B 不变量 12）：不得出现「完全权限仍无终端/网络通道」
 *   式表述；边界声明原文照落。
 */
import { useState } from "react";

import { type PermissionMode, type SessionIpc } from "./session";
import { t } from "./i18n";

export interface PermissionModeSelectorProps {
  /** 会话 IPC（仅测试/E2E 注入占位；本组件不直接调用——由父组件回调用真实 IPC）。 */
  ipc?: SessionIpc;
  /** 激活会话（`null` = 无会话 → 禁用 + 空态）。 */
  sessionId: string | null;
  /** 当前档位（`null` = 未选中/无会话）。 */
  value: PermissionMode | null;
  /** 选择回调（父组件负责调用 IPC 并处理失败）。 */
  onChange(mode: PermissionMode): void;
  disabled?: boolean;
}

/** 档位顺序与显示标签（与运行时同名同序；`workspace_write` 为缺省档）。 */
export const PERMISSION_MODE_ORDER: PermissionMode[] = [
  "read_only",
  "workspace_write",
  "danger_full_access",
];

export const PERMISSION_MODE_LABELS: Record<PermissionMode, string> = {
  read_only: "permission.mode.view_only",
  workspace_write: "permission.mode.modify_in_workspace_default",
  danger_full_access: "permission.mode.full_access",
};

/** 输入区图标按钮的短标签（弹层内保留完整标签与默认档说明）。 */
const PERMISSION_MODE_SHORT_LABELS: Record<PermissionMode, string> = {
  read_only: "permission.mode.view_only",
  workspace_write: "permission.mode.modify_in_workspace",
  danger_full_access: "permission.mode.full_access",
};

/**
 * 常驻边界声明（ADR-014 附录 F-3 定稿；R14：弹层内常驻一行，不随档位切换而消失）。
 *
 * 原文照落；任何改写都属破坏性变更（UI-UX §2.4 文案纪律）。
 */
export const PERMISSION_MODE_BOUNDARY_TEXT = "permission.mode.boundary_text";

export function PermissionModeSelector({
  ipc,
  sessionId,
  value,
  onChange,
  disabled = false,
}: PermissionModeSelectorProps) {
  const [open, setOpen] = useState(false);
  /** `ipc` 仅作测试/E2E 注入占位（本组件不直接调用；真实 IPC 由父组件回调用）。 */
  void ipc;
  const noSession = sessionId === null;
  const disabledAll = disabled || noSession;
  const visible = open && !disabledAll;

  return (
    <div
      className="permission-mode-selector"
      data-testid="permission-mode-selector"
      data-mode={value ?? ""}
      data-disabled={String(disabledAll)}
    >
      <button
        type="button"
        className={`permission-mode-toggle ic ${
          value === "read_only"
            ? "perm-read"
            : value === "danger_full_access"
              ? "perm-full"
              : value === "workspace_write"
                ? "perm-write"
                : ""
        }`}
        data-testid="permission-mode-toggle"
        aria-label={t("permission.mode.permission_mode")}
        aria-expanded={visible}
        disabled={disabledAll}
        title={t("permission.mode.permission_mode_session_level_changes")}
        onClick={() => setOpen((current) => !current)}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M12 3l7 3v6c0 4.5-3 7.5-7 9-4-1.5-7-4.5-7-9V6l7-3z" />
          <path d="M9 12l2 2 4-4" />
        </svg>
        <span className="ic-label" data-testid="permission-mode-current">
          {value !== null ? t(PERMISSION_MODE_SHORT_LABELS[value]) : t("permission.mode.no_session")}
        </span>
        <span
          className="dot"
          style={{
            background:
              value === "read_only"
                ? "var(--bl)"
                : value === "danger_full_access"
                  ? "var(--er)"
                  : value === "workspace_write"
                    ? "var(--wn)"
                    : "var(--tx3)",
          }}
        />
      </button>

      {noSession ? (
        <p className="permission-mode-empty" data-testid="permission-mode-empty" role="note">
          {t("permission.mode.no_session_select_or_create")}
        </p>
      ) : null}

      {/* 弹层内容始终渲染（`hidden` 控制显隐）——常驻边界声明可随时断言。 */}
      <div
        className="permission-mode-popover ipop ipop-perm"
        data-testid="permission-mode-popover"
        data-open={String(visible)}
        hidden={!visible}
      >
        <div className="permission-mode-head ipop-title">{t("permission.mode.agent_permissions")}</div>
        <div className="permission-mode-list" role="radiogroup" aria-label={t("permission.mode.permission_mode")}>
          {PERMISSION_MODE_ORDER.map((mode) => (
            <button
              key={mode}
              type="button"
              role="radio"
              aria-checked={mode === value}
              className={mode === value ? "permission-mode-item on" : "permission-mode-item"}
              data-testid="permission-mode-item"
              data-mode={mode}
              disabled={disabledAll}
              onClick={() => {
                onChange(mode);
                setOpen(false);
              }}
            >
              <span
                className="pdot"
                style={{
                  background:
                    mode === "read_only"
                      ? "var(--bl)"
                      : mode === "danger_full_access"
                        ? "var(--er)"
                        : "var(--wn)",
                }}
              />
              <span className="pt">{t(PERMISSION_MODE_LABELS[mode])}</span>
              {mode === value ? <span className="ck">✓</span> : null}
            </button>
          ))}
        </div>
        {/* R14：常驻一行（不随档位切换消失/清空；DOM 常驻逻辑见上）。 */}
        <p className="permission-mode-boundary" data-testid="permission-mode-boundary">
          {t(PERMISSION_MODE_BOUNDARY_TEXT)}
        </p>
      </div>
    </div>
  );
}
