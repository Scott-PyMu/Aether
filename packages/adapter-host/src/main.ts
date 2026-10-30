/** 发布形态适配器宿主：三官方运行时。 */
import { startHost } from "./common";
import { OFFICIAL_ENTRIES } from "./entries";

startHost(OFFICIAL_ENTRIES, process.argv.slice(2));
