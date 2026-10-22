/**
 * 关于页（M3-05；设计 D7/§2.1，UI-UX S-10/§7.3）。
 *
 * - 版本 / 线协议版本 / 数据目录 / 安全边界声明；
 * - Mock-only beta 标记（条件显示：仅 mock-only 路径构建传入 `mockOnly=true`；
 *   当前为 real-adapter 路径，不显示）；
 * - 诊断导出入口。
 */
import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";

export interface AboutPageProps {
  dataDir: string;
  /** Mock-only beta 标记（M2-02M DoD3；real-adapter 路径为 false）。 */
  mockOnly?: boolean;
  onBack: () => void;
  onOpenDiagnostics?: () => void;
}

export function AboutPage({
  dataDir,
  mockOnly = false,
  onBack,
  onOpenDiagnostics,
}: AboutPageProps) {
  return (
    <div className="overlay" data-testid="overlay-about">
      <section
        className="about-page modal"
        data-testid="about-page"
        role="dialog"
        aria-modal="true"
      >
        <div className="modal-h">
          <h3>关于</h3>
          <button
            type="button"
            className="modal-close"
            data-testid="overlay-back"
            aria-label="关闭关于"
            onClick={onBack}
          >
            ×
          </button>
        </div>
        <div className="about-body">
          <div className="about-box">
            <div className="about-logo">A</div>
            <div className="about-name">Aether</div>
            <div className="about-ver">
              统一多 Agent 编排平台
              <br />
              <span data-testid="about-version">版本 {APP_VERSION}</span>
              <br />
              <span data-testid="about-protocol">
                线协议 v{PROTOCOL_VERSION.major}.{PROTOCOL_VERSION.minor}（Aether 线协议）
              </span>
              <br />
              Apache-2.0 · 净室实现
            </div>
            <div className="about-tag">P0 三运行时：Claude Code / Codex / DeepSeek Harness</div>

            <p className="about-data-dir" data-testid="about-data-dir">
              数据目录：{dataDir}
            </p>

            <p className="about-boundary" data-testid="about-security-boundary">
              安全边界：MVP 为信任级模型（非沙箱）——应用本体、官方适配器与 Runtime CLI
              均视为可信一等公民；权限门防误操作与非计划行为，不防蓄意攻击；P3 起才允许
              第三方适配器（OS 沙箱 + 显式信任确认）。
            </p>

            {mockOnly ? (
              <p className="about-beta-marker" data-testid="about-beta-marker" role="alert">
                Mock-only beta：不含真实模型调用（真实适配器接入为 P1 前置）。
              </p>
            ) : null}

            <div className="about-links">
              {onOpenDiagnostics ? (
                <button
                  type="button"
                  className="about-link"
                  data-testid="about-open-diagnostics"
                  onClick={onOpenDiagnostics}
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M14 3H7a2 2 0 00-2 2v14a2 2 0 002 2h10a2 2 0 002-2V8z" />
                    <path d="M14 3v5h5" />
                  </svg>
                  导出诊断包
                </button>
              ) : null}
              <div className="about-link">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z" />
                </svg>
                第三方依赖清单（SBOM）
              </div>
              <div className="about-link">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <circle cx="12" cy="12" r="9" />
                  <path d="M12 8v4l3 2" />
                </svg>
                构建信息：commit / 时间 / 工具链
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>
  );
}
