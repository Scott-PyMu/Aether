import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { useCallback, useEffect, useState } from "react";

import { AboutPage } from "./AboutPage";
import { AuditPage } from "./AuditPage";
import { BackupPage } from "./BackupPage";
import { DiagnosticsPage } from "./DiagnosticsPage";
import { EventBridgeIndicator } from "./EventBridgeIndicator";
import { HealthMonitor } from "./HealthMonitor";
import { SessionWorkbench } from "./SessionWorkbench";
import { SettingsPage } from "./SettingsPage";
import { StartupGate } from "./StartupGate";
import { describeIpcError, fetchStartup, type StartupSnapshot } from "./startup";

/** 单窗口覆盖层视图（M3-04/M3-05；UI-UX §1.1：不使用 URL 路由）。 */
type AppView = "workbench" | "backup" | "diagnostics" | "settings" | "about" | "audit";

export function App() {
  const [startup, setStartup] = useState<StartupSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [view, setView] = useState<AppView>("workbench");
  /**
   * M7-09/ADR-026：新建会话入口当前选中的 Agent 预设（提升到 `App` 以便设置页
   * 「使用中」标记与工作台新建入口共享同一选择；`null` = 不使用预设）。
   */
  const [activePresetId, setActivePresetId] = useState<string | null>(null);
  /** M7-09/ADR-026：预设清单版本（设置页增改删后递增 → 工作台重新拉取清单）。 */
  const [presetsVersion, setPresetsVersion] = useState(0);

  useEffect(() => {
    let active = true;
    fetchStartup()
      .then((snapshot) => {
        if (active) {
          setStartup(snapshot);
        }
      })
      .catch((error: unknown) => {
        if (active) {
          setLoadError(describeIpcError(error));
        }
      });
    return () => {
      active = false;
    };
  }, []);

  const onSnapshot = useCallback((snapshot: StartupSnapshot) => {
    setStartup(snapshot);
  }, []);

  if (loadError) {
    return (
      <main className="app" data-testid="startup-load-error">
        <div className="boot-screen">
          <div className="boot-card">
            <div className="about-logo">A</div>
            <h1>Aether</h1>
            <p className="boot-error">启动自检失败：{loadError}</p>
          </div>
        </div>
      </main>
    );
  }

  if (!startup) {
    return (
      <main className="app" data-testid="startup-loading">
        <div className="boot-screen">
          <div className="boot-card">
            <div className="about-logo">A</div>
            <h1>Aether</h1>
            <p>正在执行启动自检…</p>
          </div>
        </div>
      </main>
    );
  }

  if (startup.phase !== "ready") {
    return <StartupGate snapshot={startup} onSnapshot={onSnapshot} />;
  }

  return (
    <main className="app">
      <div className="app-main">
        <SessionWorkbench
          onOpenDiagnostics={() => setView("diagnostics")}
          activePresetId={activePresetId}
          onActivePresetChange={setActivePresetId}
          presetsVersion={presetsVersion}
          onOpenSettings={() => setView("settings")}
          onOpenBackup={() => setView("backup")}
          onOpenAudit={() => setView("audit")}
          onOpenAbout={() => setView("about")}
          appVersion={APP_VERSION}
          protocolVersion={PROTOCOL_VERSION}
          dataDir={startup.data_dir}
          statusSlot={
            <>
              <HealthMonitor onOpenDiagnostics={() => setView("diagnostics")} />
              <EventBridgeIndicator />
            </>
          }
        />
        {view === "backup" ? (
          <BackupPage onBack={() => setView("workbench")} />
        ) : null}
        {view === "diagnostics" ? (
          <DiagnosticsPage onBack={() => setView("workbench")} />
        ) : null}
        {view === "audit" ? (
          <AuditPage onBack={() => setView("workbench")} />
        ) : null}
        {view === "settings" ? (
          <SettingsPage
            dataDir={startup.data_dir}
            securityLevel={startup.security_level ?? null}
            activePresetId={activePresetId}
            onActivePresetChange={setActivePresetId}
            onPresetsChanged={() => setPresetsVersion((value) => value + 1)}
            onBack={() => setView("workbench")}
            onOpenBackup={() => setView("backup")}
            onOpenDiagnostics={() => setView("diagnostics")}
            onOpenAbout={() => setView("about")}
          />
        ) : null}
        {view === "about" ? (
          <AboutPage
            dataDir={startup.data_dir}
            onBack={() => setView("workbench")}
            onOpenDiagnostics={() => setView("diagnostics")}
          />
        ) : null}
      </div>
    </main>
  );
}
