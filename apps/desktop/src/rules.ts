/**
 * 权限规则 IPC 契约（M6-01；ADR-019 / ADR-018；设计 D9 扩展）。
 *
 * 与 Rust 侧 `session_backend.rs` 的 JSON 形状一致（snake_case）：
 * - `rules_list` → 规则清单（按创建时间升序；无参数，`payload` 缺省 / `null`）；
 * - `rule_add` / `rule_update` → 新增 / 更新（非法规则拒绝且不落库）；
 * - `rule_delete` / `rule_toggle` → 删除 / 启停（`rule_id` 不存在 → `rule_not_found`）。
 *
 * 规则在档位矩阵之上叠加（deny 收紧恒允许；allow 仅免逐次审批，不越过硬边界）；
 * 本文件是规则管理页的类型化封装（可注入替身；生产 = `@aether/protocol` client）。
 */
import { client } from "@aether/protocol";

/** `rules_list` 元素（`permission_rules` 表投影；字段与表列一致）。 */
export interface RuleEntry {
  id: string;
  name: string;
  resource: string;
  action: string;
  target_kind: string;
  target: string | null;
  decision: string;
  priority: number;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

/** `rule_add` 参数（ADR-018 附录 A-1；`any` 时 `target` 省略；`priority` 缺省 0）。 */
export interface RuleAddInput {
  name: string;
  resource: string;
  action: string;
  target_kind: string;
  target?: string;
  decision: string;
  priority?: number;
}

/** `rule_update` 参数（至少 1 个变更字段；`resource` 变更须同时提供 `action`）。 */
export interface RuleUpdateInput {
  rule_id: string;
  name?: string;
  resource?: string;
  action?: string;
  target_kind?: string;
  target?: string;
  decision?: string;
  priority?: number;
}

/** 规则命令面（可注入替身；生产 = Tauri 生产实现）。 */
export interface RulesIpc {
  /** 规则清单（无参数；`payload` 缺省 / `null` 合法）。 */
  listRules(): Promise<RuleEntry[]>;
  addRule(input: RuleAddInput): Promise<RuleEntry>;
  updateRule(input: RuleUpdateInput): Promise<RuleEntry>;
  deleteRule(ruleId: string): Promise<{ rule_id: string; deleted: boolean }>;
  toggleRule(
    ruleId: string,
    enabled: boolean,
  ): Promise<{ rule_id: string; enabled: boolean }>;
}

/** 生产实现（typed client；命令入参为内层 DTO）。 */
export const rulesIpc: RulesIpc = {
  async listRules() {
    return client.rulesList<RuleEntry[]>();
  },
  async addRule(input) {
    return client.ruleAdd<RuleEntry>(input);
  },
  async updateRule(input) {
    return client.ruleUpdate<RuleEntry>(input);
  },
  async deleteRule(ruleId) {
    return client.ruleDelete<{ rule_id: string; deleted: boolean }>({
      rule_id: ruleId,
    });
  },
  async toggleRule(ruleId, enabled) {
    return client.ruleToggle<{ rule_id: string; enabled: boolean }>({
      rule_id: ruleId,
      enabled,
    });
  },
};

/** 规则资源（`permission_rules.resource` CHECK 枚举）。 */
export const RULE_RESOURCES = ["fs.read", "fs.write", "exec", "net"] as const;

export type RuleResource = (typeof RULE_RESOURCES)[number];

/** 资源展示文案（规则表单选项）。 */
export const RULE_RESOURCE_LABELS: Record<RuleResource, string> = {
  "fs.read": "文件读取（fs.read）",
  "fs.write": "文件写入（fs.write）",
  exec: "命令执行（exec）",
  net: "网络访问（net）",
};

/** `resource` → `action` 推导（ADR-019 附录 A-1：action 与 resource 匹配）。 */
export function ruleActionForResource(resource: string): string {
  switch (resource) {
    case "fs.read":
      return "read";
    case "fs.write":
      return "write";
    case "exec":
      return "exec";
    case "net":
      return "connect";
    default:
      return "";
  }
}

/** `resource` → 允许的 `target_kind`（ADR-019 附录 A-1 兼容矩阵）。 */
export function ruleTargetKindsForResource(resource: string): string[] {
  switch (resource) {
    case "fs.read":
    case "fs.write":
      return ["path", "any"];
    case "exec":
      return ["command", "any"];
    case "net":
      return ["host", "any"];
    default:
      return ["any"];
  }
}

/** `target_kind` 展示文案。 */
export const RULE_TARGET_KIND_LABELS: Record<string, string> = {
  path: "路径（path）",
  command: "命令名（command）",
  host: "主机名（host）",
  any: "全部（any）",
};

/** 规则决议（规则不产生 `ask`）。 */
export const RULE_DECISIONS = ["allow", "deny"] as const;

/** 规则决议展示文案。 */
export const RULE_DECISION_LABELS: Record<string, string> = {
  allow: "允许（allow）",
  deny: "拒绝（deny）",
};

/** 规则不存在错误码（ADR-018 附录 A-2；UI-UX §5 文案「规则不存在，已刷新列表」）。 */
export const RULE_NOT_FOUND_CODE = "rule_not_found";

/** `rule_not_found` 展示文案（UI-UX §5 文案表登记值）。 */
export const RULE_NOT_FOUND_MESSAGE = "规则不存在，已刷新列表";
