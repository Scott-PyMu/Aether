import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { useCallback, useEffect, useState } from "react";

import { StartupGate } from "./StartupGate";
import { describeIpcError, fetchStartup, type StartupSnapshot } from "./startup";

export function App() {
  const [startup, setStartup] = useState<StartupSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

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
    </main>
  );
}
