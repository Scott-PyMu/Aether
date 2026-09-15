import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { APP_VERSION, PROTOCOL_VERSION } from "@aether/protocol";
import { App } from "./App";

afterEach(() => {
  cleanup();
});

describe("App", () => {
  it("显示注入的版本号（单一版本来源）", () => {
    render(<App />);
    expect(screen.getByTestId("app-version").textContent).toBe(
      `版本 ${APP_VERSION}`,
    );
  });

  it("显示线协议版本", () => {
    render(<App />);
    expect(screen.getByTestId("protocol-version").textContent).toBe(
      `线协议 v${PROTOCOL_VERSION.major}.${PROTOCOL_VERSION.minor}`,
    );
  });
});
