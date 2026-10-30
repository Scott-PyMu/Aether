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
      // 生成物（由 aether-app 清单生成，禁止手改）不计入源码覆盖率：
      // bindings.ts/types.ts/client.ts 为契约镜像，生成物一致性由
      // `pnpm --filter @aether/protocol check` 与 `verify:m9-01-baseline` 断言。
      exclude: ["src/**/*.test.ts", "src/bindings.ts", "src/types.ts", "src/client.ts"],
      thresholds: {
        lines: linesThreshold,
      },
    },
  },
});
