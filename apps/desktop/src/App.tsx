import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { useCallback, useEffect, useState } from "react";

import { BackupPage } from "./BackupPage";
import { EventBridgeIndicator } from "./EventBridgeIndicator";
import { HealthMonitor } from "./HealthMonitor";
import { SessionWorkbench } from "./SessionWorkbench";
import { StartupGate } from "./StartupGate";
import { describeIpcError, fetchStartup, type StartupSnapshot } from "./startup";

/** 单窗口覆盖层视图（M3-04；UI-UX §1.1：不使用 URL 路由）。 */
type AppView = "workbench" | "backup";

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
      <p>
        <button
          type="button"
          data-testid="backup-open"
          onClick={() => setView("backup")}
        >
          备份与恢复
        </button>
      </p>
      <HealthMonitor />
      <EventBridgeIndicator />
      <SessionWorkbench />
      {view === "backup" ? (
        <BackupPage onBack={() => setView("workbench")} />
      ) : null}
    </main>
  );
}
