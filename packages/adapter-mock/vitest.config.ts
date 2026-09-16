import { defineConfig } from "vitest/config";

const linesThreshold = Number(process.env.AETHER_COVERAGE_LINES ?? 70);

export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "json-summary"],
      include: ["src/**/*.ts"],
      exclude: ["src/**/*.test.ts", "src/main.ts"],
      thresholds: {
        lines: linesThreshold,
      },
    },
  },
});
