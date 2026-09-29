import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { useCallback, useEffect, useState } from "react";

import { AboutPage } from "./AboutPage";
import { BackupPage } from "./BackupPage";
import { DiagnosticsPage } from "./DiagnosticsPage";
import { EventBridgeIndicator } from "./EventBridgeIndicator";
import { HealthMonitor } from "./HealthMonitor";
import { SessionWorkbench } from "./SessionWorkbench";
import { SettingsPage } from "./SettingsPage";
import { StartupGate } from "./StartupGate";
import { describeIpcError, fetchStartup, type StartupSnapshot } from "./startup";

/** 单窗口覆盖层视图（M3-04/M3-05；UI-UX §1.1：不使用 URL 路由）。 */
type AppView = "workbench" | "backup" | "diagnostics" | "settings" | "about";

export function App() {
  const [startup, setStartup] = useState<StartupSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [view, setView] = useState<AppView>("workbench");

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
      <main className="app-shell" data-testid="startup-load-error">
        <h1>Aether</h1>
        <p>启动自检失败：{loadError}</p>
      </main>
    );
  }

  if (!startup) {
    return (
      <main className="app-shell" data-testid="startup-loading">
        <p>正在执行启动自检…</p>
      </main>
    );
  }

  if (startup.phase !== "ready") {
    return <StartupGate snapshot={startup} onSnapshot={onSnapshot} />;
  }

  return (
    <main className="app-shell">
      <h1>Aether</h1>
      <p className="app-subtitle">统一多 Agent 编排平台</p>
      <p data-testid="app-version">版本 {APP_VERSION}</p>
      <p data-testid="protocol-version">
        线协议 v{PROTOCOL_VERSION.major}.{PROTOCOL_VERSION.minor}
      </p>
      <p className="app-data-dir" data-testid="app-data-dir">
        数据目录：{startup.data_dir}
      </p>
      <p className="app-nav">
        <button
          type="button"
          data-testid="backup-open"
          onClick={() => setView("backup")}
        >
          备份与恢复
        </button>
        <button
          type="button"
          data-testid="settings-open"
          onClick={() => setView("settings")}
        >
          设置
        </button>
        <button
          type="button"
          data-testid="diagnostics-open"
          onClick={() => setView("diagnostics")}
        >
          诊断
        </button>
        <button
          type="button"
          data-testid="about-open"
          onClick={() => setView("about")}
        >
          关于
        </button>
      </p>
      <HealthMonitor onOpenDiagnostics={() => setView("diagnostics")} />
      <EventBridgeIndicator />
      <SessionWorkbench onOpenDiagnostics={() => setView("diagnostics")} />
      {view === "backup" ? (
        <BackupPage onBack={() => setView("workbench")} />
      ) : null}
      {view === "diagnostics" ? (
        <DiagnosticsPage onBack={() => setView("workbench")} />
      ) : null}
      {view === "settings" ? (
        <SettingsPage
          dataDir={startup.data_dir}
          securityLevel={startup.security_level ?? null}
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
    </main>
  );
}
