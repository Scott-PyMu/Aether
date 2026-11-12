/**
 * 关于页（M3-05；设计 D7/§2.1，UI-UX S-10/§7.3）。
 *
 * - 版本 / 线协议版本 / 数据目录 / 安全边界声明；
 * - Mock-only beta 标记（条件显示：仅 mock-only 路径构建传入 `mockOnly=true`；
 *   当前为 real-adapter 路径，不显示）；
 * - 诊断导出入口。
 */
import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { t } from "./i18n";

export interface AboutPageProps {
  /** 数据目录（M9 起由 S-12 承接；主 UI 可选展示）。 */
  dataDir?: string;
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
          <h3>{t("app.common.about")}</h3>
          <button
            type="button"
            className="modal-close"
            data-testid="overlay-back"
            aria-label={t("app.about.close_about")}
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
              {t("app.about.unified_multi_agent_orchestration_platform")}
              <br />
              <span data-testid="about-version">{t("app.common.version")} {APP_VERSION}</span>
              <br />
              <span data-testid="about-protocol">
                {t("app.common.wire_protocol_v")}{PROTOCOL_VERSION.major}.{PROTOCOL_VERSION.minor}{t("app.about.aether_wire_protocol")}
              </span>
              <br />
              {t("app.about.apache_2_0_clean_room")}
            </div>
            <div className="about-tag">{t("app.common.p0_runtimes_claude_code_codex")}</div>

            {dataDir ? (
              <p className="about-data-dir" data-testid="about-data-dir">
                {t("app.common.data_directory")}{dataDir}
              </p>
            ) : null}

            <p className="about-boundary" data-testid="about-security-boundary">
              {t("app.about.security_boundary_the_mvp_uses")}
            </p>

            {mockOnly ? (
              <p className="about-beta-marker" data-testid="about-beta-marker" role="alert">
                {t("app.about.mock_only_beta_no_real")}
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
                  {t("app.action.export_diagnostics_bundle")}
                </button>
              ) : null}
              <div className="about-link">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z" />
                </svg>
                {t("app.about.third_party_dependency_list_sbom")}
              </div>
              <div className="about-link">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <circle cx="12" cy="12" r="9" />
                  <path d="M12 8v4l3 2" />
                </svg>
                {t("app.about.build_info_commit_time_toolchain")}
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>
  );
}
