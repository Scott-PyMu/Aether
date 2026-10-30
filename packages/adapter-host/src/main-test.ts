/** 测试构建适配器宿主：三官方 + Mock（仅安装冒烟/E2E；ADR-008）。 */
import { runCli as runMock } from "@aether/adapter-mock/src/cli";

import { startHost } from "./common";
import { OFFICIAL_ENTRIES } from "./entries";

startHost(
  {
    ...OFFICIAL_ENTRIES,
    mock: { prefix: "[adapter-mock]", run: runMock },
  },
  process.argv.slice(2),
);
