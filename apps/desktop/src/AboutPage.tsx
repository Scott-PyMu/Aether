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
      <section className="about-page" data-testid="about-page" role="dialog" aria-modal="true">
        <header className="about-header">
          <h2>关于 Aether</h2>
          <button type="button" data-testid="overlay-back" onClick={onBack}>
            返回工作台
          </button>
        </header>

        <p data-testid="about-version">版本 {APP_VERSION}</p>
        <p data-testid="about-protocol">
          线协议 v{PROTOCOL_VERSION.major}.{PROTOCOL_VERSION.minor}（Aether 线协议）
        </p>
        <p data-testid="about-data-dir">数据目录：{dataDir}</p>

        <p data-testid="about-security-boundary">
          安全边界：MVP 为信任级模型（非沙箱）——应用本体、官方适配器与 Runtime CLI
          均视为可信一等公民；权限门防误操作与非计划行为，不防蓄意攻击；P3 起才允许
          第三方适配器（OS 沙箱 + 显式信任确认）。
        </p>

        {mockOnly ? (
          <p className="about-beta-marker" data-testid="about-beta-marker" role="alert">
            Mock-only beta：不含真实模型调用（真实适配器接入为 P1 前置）。
          </p>
        ) : null}

        <section className="about-actions">
          {onOpenDiagnostics ? (
            <button
              type="button"
              data-testid="about-open-diagnostics"
              onClick={onOpenDiagnostics}
            >
              导出诊断
            </button>
          ) : null}
        </section>
      </section>
    </div>
  );
}
