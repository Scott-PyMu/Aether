import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const linesThreshold = Number(process.env.AETHER_COVERAGE_LINES ?? 70);

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    // 性能基准用例（m3-02/m7-04）对同机并发敏感；测试文件串行执行，
    // 避免多个 jsdom/transform 线程争抢导致帧预算误报（不改变断言/DoD）。
    fileParallelism: false,
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
