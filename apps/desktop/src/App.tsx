import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";

export function App() {
  return (
    <main className="app-shell">
      <h1>Aether</h1>
      <p className="app-subtitle">统一多 Agent 编排平台</p>
      <p data-testid="app-version">版本 {APP_VERSION}</p>
      <p data-testid="protocol-version">
        线协议 v{PROTOCOL_VERSION.major}.{PROTOCOL_VERSION.minor}
      </p>
    </main>
  );
}
