import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const linesThreshold = Number(process.env.AETHER_COVERAGE_LINES ?? 70);

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    coverage: {
      provider: "v8",
      reporter: ["text", "json-summary"],
      include: ["src/**/*.{ts,tsx}"],
      exclude: ["src/main.tsx", "src/vite-env.d.ts", "src/**/*.test.{ts,tsx}"],
      thresholds: {
        lines: linesThreshold,
      },
    },
  },
});
