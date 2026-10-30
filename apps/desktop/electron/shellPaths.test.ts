import path from "node:path";
import { describe, expect, it } from "vitest";

import { shellAssetCandidates } from "./shellPaths";

describe("shellAssetCandidates", () => {
  it("打包布局优先 app.asar.unpacked（file:// 不读 asar）", () => {
    const resources = path.join("C:", "Programs", "Aether", "resources");
    const dirname = path.join(resources, "app.asar", "apps", "desktop", "electron", "dist");
    const candidates = shellAssetCandidates({
      page: "startup",
      file: "startup.html",
      isPackaged: true,
      resourcesPath: resources,
      dirname,
    });
    expect(candidates[0]).toBe(
      path.join(
        resources,
        "app.asar.unpacked",
        "apps",
        "desktop",
        "electron",
        "dist",
        "shell",
        "startup",
        "startup.html",
      ),
    );
    expect(candidates[1]).toBe(path.join(dirname, "shell", "startup", "startup.html"));
    expect(candidates[2]).toBe(path.join(dirname, "..", "startup", "startup.html"));
  });

  it("开发布局不含 app.asar.unpacked", () => {
    const dirname = path.join("E:", "Aether", "apps", "desktop", "electron", "dist");
    const candidates = shellAssetCandidates({
      page: "fatal",
      file: "fatal.html",
      isPackaged: false,
      resourcesPath: path.join("E:", "Aether", "resources"),
      dirname,
    });
    expect(candidates.some((candidate) => candidate.includes("app.asar.unpacked"))).toBe(false);
    expect(candidates[0]).toBe(path.join(dirname, "shell", "fatal", "fatal.html"));
  });
});
