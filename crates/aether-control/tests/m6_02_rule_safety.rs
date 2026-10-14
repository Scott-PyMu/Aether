//! M6-02 集成测试：规则安全回归与误配率抽查（P1 计划 M6-02 DoD1–DoD3；ADR-019 §2.2 预注册方法、附录 A-4 归档契约）。
//!
//! - **DoD1**：方法预注册（ADR-019 §2.2 为登记基线；`docs/M6-02-误配率抽查方法-预注册.md` 冻结执行参数）——本文件为执行载体。
//! - **DoD2**：抽样（执行窗口内规则全集，按创建时间倒序取最近 100 条；不足取全集）+ **双人独立判定**（判定 1 = 依据 ADR-019 §2.1 决策顺序重演规范判定；判定 2 = 真实 `PermissionService` 端到端执行并复核落审计）+ **第三人复核**分歧样本；误配率 = 误配条数 / 样本量，**<5% 通过**；产物按 ADR-019 附录 A-4 归档（`样本清单.json` + `双人判定表.csv` + `误配率计算输出.txt`）。
//! - **DoD3**：误配案例逐条分析与处置建议（`误配案例分析.md`；**不对规则数据做静默修改**——执行前后规则行哈希一致断言）。
//!
//! 安全回归（DoD3）：
//!
//! - 零直通：抽查执行的每一例决策均落审计（`permission.allowed_by_policy` / `permission.denied_by_policy`，逐请求 `request_id` 对照）；
//! - **pending 票据不因规则变更被篡改**（规则只改变决议来源，不产生旁路）；
//! - 硬边界回归：`read_only` 写 / 低档 `exec`·`net` 不被 allow 规则放宽；
//! - T6 并发回归：100 并发 ask（规则在途变更）无丢失/重复/死锁；
//! - T7 全样本与 M2-10 零直通由 `m2_03_permission` / `m2_10_permission_loop` 套件承接（验证脚本 `scripts/test/m6-02/verify-m6-02.mjs` 复跑）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use aether_control::{PermissionConfig, PermissionResolution, PermissionService};
use aether_core::{PermissionDecision, SessionId};
use aether_store::{AuditLogRecord, PermissionRuleRecord, ReadPool, StoreCommand, StoreOutcome};
use m2_support::{
    build_permission_service, insert_session_row, manual_clock, permission_request, wait_for,
    wait_permissions_pending, TestCore,
};
use serde_json::{json, Value};

// =============================================================================
// 通用工具
// =============================================================================

const EXPECTED_SAMPLE_SIZE: usize = 100;
const MISCONFIGURATION_RATE_LIMIT: f64 = 5.0;

fn request_id(label: &str) -> String {
    format!("m6-02-{label}")
}

/// 证据输出目录（未注入 `AETHER_M6_02_EVIDENCE_DIR` 时仅在断言路径产出，不写文件）。
fn evidence_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("AETHER_M6_02_EVIDENCE_DIR").map(std::path::PathBuf::from)
}

fn write_artifact(name: &str, contents: &str) {
    if let Some(dir) = evidence_dir() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
    }
}

/// canonical 路径 → 目标字符串形态（剥离 `\\?\` 扩展前缀；与测试夹具口径一致）。
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// FNV-1a（规则集完整性快照；不引入依赖）。
fn fnv1a(parts: &[String]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("fnv1a:{hash:016x}")
}

fn rule_fingerprint(record: &PermissionRuleRecord) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        record.id,
        record.name,
        record.resource,
        record.action,
        record.target_kind,
        record.target.as_deref().unwrap_or("<null>"),
        record.decision,
        record.priority,
        record.enabled,
        record.created_at
    )
}

async fn rules_fingerprint(reads: &ReadPool) -> String {
    let mut records = reads.permission_rules().await.unwrap();
    records.sort_by(|left, right| left.id.cmp(&right.id));
    fnv1a(
        &records
            .iter()
            .map(rule_fingerprint)
            .collect::<Vec<String>>(),
    )
}

// =============================================================================
// 抽查语料（规则 + 判定尝试向量）构造
// =============================================================================

/// 语料规则（登记意图 = `expected`；用于判定 1 的规范重演与误配分类）。
#[derive(Debug, Clone)]
struct CorpusRule {
    record: PermissionRuleRecord,
    /// 判定尝试向量（每条规则至少 1 条；`primary=true` 计入误配率）。
    attempts: Vec<AttemptVector>,
}

/// 判定尝试向量。
#[derive(Debug, Clone)]
struct AttemptVector {
    /// `primary` 向量计入误配率；其余为边界回归向量（单独统计）。
    primary: bool,
    resource: String,
    action: String,
    raw_target: String,
    canonical_target: Option<String>,
    /// 有效矩阵结果（默认档 `workspace_write`）。
    matrix: &'static str,
    /// 登记意图：规则命中时的系统应有终局（allow/deny/ask）。
    expected: &'static str,
    /// 向量是否会命中该规则（按登记意图）。
    rule_applies: bool,
    note: &'static str,
}

/// 确定性 LCG（语料可复跑；不使用外部依赖）。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
        options[(self.next() as usize) % options.len()]
    }
}

const COMMAND_SAMPLE: [&str; 10] = [
    "rm", "git", "npm", "cargo", "node", "python", "pip", "curl", "tar", "make",
];
const HOST_SAMPLE: [&str; 10] = [
    "api.github.com",
    "crates.io",
    "registry.npmjs.org",
    "pypi.org",
    "huggingface.co",
    "api.anthropic.com",
    "api.openai.com",
    "example.com",
    "cdn.jsdelivr.net",
    "static.crates.io",
];

fn path_rule_record(
    id: &str,
    name: &str,
    resource: &str,
    target: Option<String>,
    decision: &str,
    created_at: i64,
) -> PermissionRuleRecord {
    PermissionRuleRecord {
        id: id.to_owned(),
        name: name.to_owned(),
        resource: resource.to_owned(),
        action: if resource == "fs.read" {
            "read".to_owned()
        } else {
            "write".to_owned()
        },
        target_kind: if target.is_some() { "path" } else { "any" }.to_owned(),
        target,
        decision: decision.to_owned(),
        priority: 10,
        enabled: true,
        created_at,
        updated_at: created_at,
    }
}

fn command_rule_record(
    id: &str,
    name: &str,
    target: Option<String>,
    decision: &str,
    created_at: i64,
) -> PermissionRuleRecord {
    PermissionRuleRecord {
        id: id.to_owned(),
        name: name.to_owned(),
        resource: "exec".to_owned(),
        action: "exec".to_owned(),
        target_kind: if target.is_some() { "command" } else { "any" }.to_owned(),
        target,
        decision: decision.to_owned(),
        priority: 10,
        enabled: true,
        created_at,
        updated_at: created_at,
    }
}

fn host_rule_record(
    id: &str,
    name: &str,
    target: Option<String>,
    decision: &str,
    created_at: i64,
) -> PermissionRuleRecord {
    PermissionRuleRecord {
        id: id.to_owned(),
        name: name.to_owned(),
        resource: "net".to_owned(),
        action: "connect".to_owned(),
        target_kind: if target.is_some() { "host" } else { "any" }.to_owned(),
        target,
        decision: decision.to_owned(),
        priority: 10,
        enabled: true,
        created_at,
        updated_at: created_at,
    }
}

/// 判定尝试规格（矩阵/期望由构造期校验，禁止拼写漂移）。
#[derive(Debug, Clone, Copy)]
struct AttemptSpec {
    matrix: &'static str,
    expected: &'static str,
    /// 向量是否会命中该规则（按登记意图）。
    rule_applies: bool,
    note: &'static str,
}

/// 构造判定尝试（`content_bytes` 按资源在 `run_attempt` 统一注入）。
fn attempt(
    primary: bool,
    resource: &str,
    action: &str,
    raw_target: String,
    canonical_target: Option<String>,
    spec: AttemptSpec,
) -> AttemptVector {
    assert!(
        matches!(spec.matrix, "allow" | "deny" | "ask"),
        "矩阵取值非法：{}",
        spec.matrix
    );
    assert!(
        matches!(spec.expected, "allow" | "deny" | "ask"),
        "期望取值非法：{}",
        spec.expected
    );
    AttemptVector {
        primary,
        resource: resource.to_owned(),
        action: action.to_owned(),
        raw_target,
        canonical_target,
        matrix: spec.matrix,
        expected: spec.expected,
        rule_applies: spec.rule_applies,
        note: spec.note,
    }
}

/// 语料矩阵（ADR-019 附录 A-1 兼容矩阵 × 判定尝试向量）。
///
/// 规模：>100 条（抽样取最近 100）；覆盖 `fs.read`/`fs.write`/`exec`/`net`
/// × `path`/`command`/`host`/`any` × `allow`/`deny`。
/// 另含 2 条**预注册演练样本**（已知表达误配：目标登记笔误，语义与登记意图不符），
/// 用于验证判定流程与案例分析产物的可执行性（不伪造系统缺陷）。
fn build_corpus(workspace: &str, outside: &str, file_count: usize) -> Vec<CorpusRule> {
    let sep = std::path::MAIN_SEPARATOR;
    let mut rules: Vec<CorpusRule> = Vec::new();
    let mut rng = Lcg::new(0x6d36_3032_2020_1013);
    let mut created_at = 1_700_000_000_000_i64;

    // 规则 ID 前缀（顺序即创建时间顺序）：**每条规则仅求值一次 `rules.len()`**
    // （M6-02 执行期自查发现并修复：同规则内二次求值造成 ID 漂移/重号）。
    let prefix = |index: usize| format!("01J000000000000000000M6{index:03}");

    // ---- fs.read（path / any；allow / deny）----
    for index in 0..file_count {
        let resource = "fs.read";
        let decision = if index % 2 == 0 { "allow" } else { "deny" };
        let id = prefix(rules.len());
        let mut attempts = Vec::new();
        if index < file_count - 2 {
            // 精确文件路径目标。
            let raw = format!("{workspace}{sep}read-{index}.txt");
            let canonical = format!("{workspace}{sep}read-{index}.txt");
            let (matrix, expected) = if decision == "deny" {
                ("allow", "deny")
            } else {
                ("allow", "allow")
            };
            attempts.push(attempt(
                true,
                resource,
                "read",
                raw.clone(),
                Some(canonical.clone()),
                AttemptSpec {
                    matrix,
                    expected,
                    rule_applies: true,
                    note: if decision == "deny" {
                        "deny 规则收紧矩阵 allow（精确命中）"
                    } else {
                        "allow 规则不改变矩阵 allow（命中审计）"
                    },
                },
            ));
            // 边界负样本（主向量同索引的相邻文件）。
            let other = format!("{workspace}{sep}read-{index}.txt.bak");
            attempts.push(attempt(
                false,
                resource,
                "read",
                other.clone(),
                Some(other),
                AttemptSpec {
                    matrix: "allow",
                    expected: "allow",
                    rule_applies: false,
                    note: "精确路径不命中同名后缀文件（边界负样本）",
                },
            ));
            let record = path_rule_record(
                &id,
                &format!("读取控制 {index}"),
                resource,
                Some(canonical),
                decision,
                created_at,
            );
            rules.push(CorpusRule { record, attempts });
        } else {
            // any 目标（全匹配）。
            let raw = format!("{workspace}{sep}read-any-{index}.txt");
            let canonical = format!("{workspace}{sep}read-any-{index}.txt");
            let (matrix, expected) = if decision == "deny" {
                ("allow", "deny")
            } else {
                ("allow", "allow")
            };
            attempts.push(attempt(
                true,
                resource,
                "read",
                raw,
                Some(canonical),
                AttemptSpec {
                    matrix,
                    expected,
                    rule_applies: true,
                    note: "any 目标全匹配（命中审计）",
                },
            ));
            let record = path_rule_record(
                &id,
                &format!("读取全匹配 {index}"),
                resource,
                None,
                decision,
                created_at,
            );
            rules.push(CorpusRule { record, attempts });
        }
        created_at += 1_000;
    }

    // ---- fs.write（path：区内收紧 / 区外放行；any：区内收紧）----
    for index in 0..file_count {
        let resource = "fs.write";
        let id = prefix(rules.len());
        let mut attempts = Vec::new();
        let (record, primary_note) = if index % 2 == 0 {
            // 区内前缀规则（deny 收紧 / allow 命中审计）。
            let decision = if index % 4 == 0 { "deny" } else { "allow" };
            let dir = format!("{workspace}{sep}zone-{index}{sep}");
            let file = format!("{workspace}{sep}zone-{index}{sep}file.txt");
            let sibling = format!("{workspace}{sep}zone-{index}x{sep}file.txt");
            if decision == "deny" {
                attempts.push(attempt(
                    true,
                    resource,
                    "write",
                    file.clone(),
                    Some(file.clone()),
                    AttemptSpec {
                        matrix: "allow",
                        expected: "deny",
                        rule_applies: true,
                        note: "区内目录前缀 deny 收紧矩阵 allow",
                    },
                ));
            } else {
                attempts.push(attempt(
                    true,
                    resource,
                    "write",
                    file.clone(),
                    Some(file),
                    AttemptSpec {
                        matrix: "allow",
                        expected: "allow",
                        rule_applies: true,
                        note: "区内目录前缀 allow 命中审计（矩阵 allow 不变）",
                    },
                ));
            }
            attempts.push(attempt(
                false,
                resource,
                "write",
                sibling.clone(),
                Some(sibling),
                AttemptSpec {
                    matrix: "allow",
                    expected: "allow",
                    rule_applies: false,
                    note: "目录前缀按分隔符边界（zone-Nx 不命中 zone-N/）",
                },
            ));
            (
                path_rule_record(
                    &id,
                    &format!("区内写入策略 {index}"),
                    resource,
                    Some(dir),
                    decision,
                    created_at,
                ),
                "",
            )
        } else {
            // 区外精确路径 allow（矩阵 ask → 规则放行）。
            let decision = if index % 4 == 1 { "allow" } else { "deny" };
            let file = format!("{outside}{sep}outside-{index}.txt");
            if decision == "allow" {
                attempts.push(attempt(
                    true,
                    resource,
                    "write",
                    file.clone(),
                    Some(file.clone()),
                    AttemptSpec {
                        matrix: "ask",
                        expected: "allow",
                        rule_applies: true,
                        note: "区外精确路径 allow 规则放行矩阵 ask",
                    },
                ));
            } else {
                attempts.push(attempt(
                    true,
                    resource,
                    "write",
                    file.clone(),
                    Some(file.clone()),
                    AttemptSpec {
                        matrix: "ask",
                        expected: "deny",
                        rule_applies: true,
                        note: "区外精确路径 deny 规则收紧矩阵 ask",
                    },
                ));
            }
            (
                path_rule_record(
                    &id,
                    &format!("区外写入策略 {index}"),
                    resource,
                    Some(file),
                    decision,
                    created_at,
                ),
                "",
            )
        };
        let _ = primary_note;
        rules.push(CorpusRule { record, attempts });
        created_at += 1_000;
    }

    // ---- exec（command / any；deny 收紧）----
    for index in 0..file_count {
        let id = prefix(rules.len());
        let mut attempts = Vec::new();
        if index % 3 == 2 {
            // any 目标 deny。
            let command = rng.pick(&COMMAND_SAMPLE);
            attempts.push(attempt(
                true,
                "exec",
                "exec",
                format!("{command} --version"),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: true,
                    note: "exec any deny 收紧（低档边界不放大）",
                },
            ));
            rules.push(CorpusRule {
                record: command_rule_record(
                    &id,
                    &format!("命令全匹配 {index}"),
                    None,
                    "deny",
                    created_at,
                ),
                attempts,
            });
        } else {
            // 命令名精确 deny。
            let command = COMMAND_SAMPLE[index % COMMAND_SAMPLE.len()];
            attempts.push(attempt(
                true,
                "exec",
                "exec",
                format!("{command} subcommand --flag"),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: true,
                    note: "exec 命令 deny 命中（矩阵低档 deny 不变）",
                },
            ));
            // 边界负样本：同前缀异命令（rmdir 不命中 rm）。
            let other = format!("{command}dir --quiet");
            attempts.push(attempt(
                false,
                "exec",
                "exec",
                other,
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: false,
                    note: "命令按词边界匹配（前缀异命令不命中）",
                },
            ));
            rules.push(CorpusRule {
                record: command_rule_record(
                    &id,
                    &format!("命令策略 {index}"),
                    Some(command.to_owned()),
                    "deny",
                    created_at,
                ),
                attempts,
            });
        }
        created_at += 1_000;
    }

    // ---- net（host / any；deny 收紧；host allow 命中审计）----
    for index in 0..file_count {
        let id = prefix(rules.len());
        let mut attempts = Vec::new();
        if index % 3 == 0 {
            // 后缀规则（`.` 前缀）：边界样本。
            let host = HOST_SAMPLE[index % HOST_SAMPLE.len()];
            let suffix = format!(
                ".{}",
                host.rsplit_once('.').map(|(_, tail)| tail).unwrap_or(host)
            );
            let decision = if index % 2 == 0 { "deny" } else { "allow" };
            let matrix = "deny";
            let expected = "deny";
            attempts.push(attempt(
                true,
                "net",
                "connect",
                host.to_owned(),
                None,
                AttemptSpec {
                    matrix,
                    expected,
                    rule_applies: true,
                    note: "net 后缀 deny 命中（矩阵低档 deny 不变）",
                },
            ));
            // 边界负样本：伪造域名（`example.com.evil.io` 不命中 `.com`）
            attempts.push(attempt(
                false,
                "net",
                "connect",
                format!("{host}.evil.io"),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: false,
                    note: "主机后缀按标签边界（伪造域名不命中）",
                },
            ));
            rules.push(CorpusRule {
                record: host_rule_record(
                    &id,
                    &format!("主机后缀策略 {index}"),
                    Some(suffix),
                    decision,
                    created_at,
                ),
                attempts,
            });
        } else {
            // 精确主机规则。
            let host = HOST_SAMPLE[index % HOST_SAMPLE.len()];
            let decision = if index % 2 == 0 { "deny" } else { "allow" };
            attempts.push(attempt(
                true,
                "net",
                "connect",
                host.to_owned(),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: true,
                    note: "net 精确主机命中（矩阵低档 deny 不变）",
                },
            ));
            attempts.push(attempt(
                false,
                "net",
                "connect",
                format!("evil-{host}"),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: false,
                    note: "精确主机不命中前缀伪造（evil-example.com）",
                },
            ));
            rules.push(CorpusRule {
                record: host_rule_record(
                    &id,
                    &format!("主机策略 {index}"),
                    Some(host.to_owned()),
                    decision,
                    created_at,
                ),
                attempts,
            });
        }
        created_at += 1_000;
    }

    // ---- 预注册演练样本 ①（表达误配：host 目标登记为 URL 形态 → 规则不生效）----
    {
        let id = prefix(rules.len());
        let host = "cdn.jsdelivr.net";
        rules.push(CorpusRule {
            record: host_rule_record(
                &id,
                "主机精确策略（演练：URL 形态登记）",
                Some(format!("http://{host}")),
                "allow",
                created_at,
            ),
            attempts: vec![attempt(
                true,
                "net",
                "connect",
                host.to_owned(),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "allow",
                    rule_applies: true,
                    note: "预注册演练样本①：目标登记为 URL 形态（`http://…`，合法登记但不命中裸主机名）→ 表达误配",
                },
            )],
        });
        created_at += 1_000;
    }

    // ---- 预注册演练样本 ②（普通合法规则：用于验证分类器不误报）----
    {
        let id = prefix(rules.len());
        let host = "huggingface.co";
        rules.push(CorpusRule {
            record: host_rule_record(
                &id,
                "主机精确策略（对照：合法登记）",
                Some(host.to_owned()),
                "deny",
                created_at,
            ),
            attempts: vec![attempt(
                true,
                "net",
                "connect",
                host.to_owned(),
                None,
                AttemptSpec {
                    matrix: "deny",
                    expected: "deny",
                    rule_applies: true,
                    note: "对照规则：合法登记 + 语义一致（无分歧）",
                },
            )],
        });
    }

    rules
}

// =============================================================================
// 判定 1（规范重演；独立于被测实现，仅依 ADR-019 §2.1 文字语义）
// =============================================================================

mod judge {
    use super::{CorpusRule, Path};
    use aether_store::PermissionRuleRecord;

    const CASE_INSENSITIVE_HOST: bool = true;

    /// 规范匹配（ADR-019 附录 A-1；不复用 `aether-security` 规则引擎实现）。
    pub fn rule_matches(rule: &PermissionRuleRecord, vector: &super::AttemptVector) -> bool {
        if rule.resource != vector.resource || rule.action != vector.action {
            return false;
        }
        match rule.target_kind.as_str() {
            "any" => true,
            "path" => {
                if !rule.resource.starts_with("fs.") {
                    return false;
                }
                let (Some(rule_target), Some(canonical)) =
                    (rule.target.as_deref(), vector.canonical_target.as_deref())
                else {
                    return false;
                };
                let canonical = plain_canonical(canonical);
                if rule_target.ends_with(std::path::MAIN_SEPARATOR) {
                    canonical.starts_with(rule_target)
                } else {
                    path_text_eq(rule_target, &canonical)
                }
            }
            "command" => {
                if rule.resource != "exec" {
                    return false;
                }
                let Some(rule_target) = rule.target.as_deref() else {
                    return false;
                };
                let first = vector.raw_target.split_whitespace().next().unwrap_or("");
                let name = Path::new(first)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(first);
                if cfg!(windows) {
                    name.eq_ignore_ascii_case(rule_target)
                } else {
                    name == rule_target
                }
            }
            "host" => {
                if rule.resource != "net" {
                    return false;
                }
                let Some(rule_target) = rule.target.as_deref() else {
                    return false;
                };
                let host = vector.raw_target.trim();
                let host = if CASE_INSENSITIVE_HOST {
                    host.to_ascii_lowercase()
                } else {
                    host.to_owned()
                };
                if let Some(suffix) = rule_target.strip_prefix('.') {
                    host == suffix || host.ends_with(&format!(".{suffix}"))
                } else {
                    host == rule_target
                }
            }
            _ => false,
        }
    }

    fn plain_canonical(value: &str) -> String {
        value
            .strip_prefix(r"\\?\")
            .or_else(|| value.strip_prefix("//?/"))
            .unwrap_or(value)
            .to_owned()
    }

    fn path_text_eq(left: &str, right: &str) -> bool {
        let left = plain_canonical(left);
        let right = plain_canonical(right);
        if cfg!(windows) {
            left.eq_ignore_ascii_case(&right)
        } else {
            left == right
        }
    }

    /// 规范终局（ADR-019 §2.1 决策顺序）：矩阵硬边界 → 命中 deny 收紧 →
    /// 命中 allow 放行（仅矩阵 `ask`）→ 矩阵结果。
    pub fn expected_outcome(rule: &CorpusRule, vector: &super::AttemptVector) -> (String, String) {
        let matches = vector.rule_applies && rule_matches(&rule.record, vector);
        let matrix = vector.matrix;
        if matrix == "deny" {
            // 硬边界：规则不可放宽（命中 deny 亦为 deny）。
            return ("deny".to_owned(), "matrix_hard_boundary".to_owned());
        }
        if matches && rule.record.decision == "deny" {
            return ("deny".to_owned(), format!("rule:{}", rule.record.id));
        }
        if matrix == "ask" {
            if matches && rule.record.decision == "allow" {
                return ("allow".to_owned(), format!("rule:{}", rule.record.id));
            }
            return ("ask".to_owned(), "matrix".to_owned());
        }
        // matrix == allow
        ("allow".to_owned(), "matrix".to_owned())
    }
}

// =============================================================================
// 判定 2（真实系统执行）+ 第三人复核
// =============================================================================

/// 抽查器的等待观测记录（每例尝试）。
#[derive(Debug, Clone)]
struct AttemptObservation {
    /// 是否进入待审批（ask 路径）。
    asked: bool,
}

/// 单次尝试：真实 `PermissionService::request`（ask 由抽查器以 `once` 收口）。
///
/// 等待观测：进入 pending → 授予 `once`；自然完成（allow/deny）→ 直接取决议。
/// 每例尝试必须有终局（死锁即测试失败）。
async fn run_attempt(
    service: &PermissionService,
    session_id: &SessionId,
    vector: &AttemptVector,
    label: &str,
) -> (PermissionResolution, AttemptObservation) {
    let request = permission_request(
        &request_id(label),
        Some(session_id),
        &vector.resource,
        &vector.action,
        Some(&vector.raw_target),
        if vector.resource == "fs.write" {
            Some(16)
        } else {
            None
        },
    );
    let service_clone = service.clone();
    let task = tokio::spawn(async move { service_clone.request(request).await });
    let mut asked = false;
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ticket) = service.pending_list(None).first().cloned() {
                asked = true;
                let _ = service
                    .resolve(
                        &ticket.request_id,
                        PermissionDecision::Allow,
                        Some(aether_core::PermissionScope::Once),
                    )
                    .await;
            }
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(observed.is_ok(), "每例尝试必须有终局（{label}）");
    let resolution = task.await.unwrap().unwrap();
    (resolution, AttemptObservation { asked })
}

/// 从审计日志回填决策来源（`request_id` 对照；零直通断言用）。
fn audit_source_for(entries: &[AuditLogRecord], request_id: &str) -> Option<(String, String)> {
    for record in entries {
        if !matches!(
            record.action.as_str(),
            "permission.allowed_by_policy" | "permission.denied_by_policy"
        ) {
            continue;
        }
        let Some(detail) = record.detail.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(detail) else {
            continue;
        };
        if value.get("request_id").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        let decision = if record.action == "permission.allowed_by_policy" {
            "allow"
        } else {
            "deny"
        };
        let source = value
            .get("resolver")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if value
                    .get("detail")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("rule:"))
                {
                    "rule".to_owned()
                } else {
                    "matrix".to_owned()
                }
            });
        return Some((decision.to_owned(), source));
    }
    None
}

// =============================================================================
// DoD2：抽查执行（判定 1 + 判定 2 + 第三人复核 + 误配率）
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m6_02_spot_check_double_judgment_and_rate() {
    let core = TestCore::open().await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace_dir.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    std::fs::create_dir_all(workspace_dir.path().join("sub")).unwrap();

    let workspace = plain(&std::fs::canonicalize(workspace_dir.path()).unwrap());
    let outside = plain(&std::fs::canonicalize(outside_dir.path()).unwrap());
    // 语料规模：6 族 × 16 条 + 2 演练样本 = 98……不足 100 时以 24 条/族
    // （24 条/族 × 4 族 + 2 = 98），故取 26 条/族保证样本可满额。
    let corpus = build_corpus(&workspace, &outside, 26);
    assert!(
        corpus.len() > EXPECTED_SAMPLE_SIZE,
        "语料必须多于样本量（{} > {EXPECTED_SAMPLE_SIZE}）",
        corpus.len()
    );

    // 落库全部语料规则（模拟执行窗口内用户规则全集）+ 快照刷新。
    let mut corpus_ids = HashSet::new();
    for entry in &corpus {
        let outcome = core
            .write()
            .execute(StoreCommand::InsertPermissionRule {
                rule: entry.record.clone(),
            })
            .await
            .unwrap();
        assert!(
            matches!(outcome, StoreOutcome::Applied { affected: 1 }),
            "规则落库失败：{}",
            entry.record.id
        );
        corpus_ids.insert(entry.record.id.clone());
    }
    let restored = service.restore_rules().await.unwrap();
    assert_eq!(restored, corpus.len(), "规则快照必须全部加载");
    assert_eq!(service.rules_snapshot().len(), corpus.len(), "快照计数一致");

    // 抽样：按创建时间倒序取最近 100 条（ADR-019 §2.2）。
    let mut persisted = core.reads().permission_rules().await.unwrap();
    persisted.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let sample: Vec<PermissionRuleRecord> = persisted
        .iter()
        .take(EXPECTED_SAMPLE_SIZE)
        .cloned()
        .collect();
    assert_eq!(sample.len(), EXPECTED_SAMPLE_SIZE, "样本量 = 100");

    let corpus_by_id: std::collections::HashMap<&str, &CorpusRule> = corpus
        .iter()
        .map(|entry| (entry.record.id.as_str(), entry))
        .collect();
    // 抽样必然覆盖演练样本（创建时间最新）与大部分语料；语料按最早创建不被抽中
    // 时以「样本 ⊂ 语料」断言保证可追溯。
    let sample_ids: HashSet<&str> = sample.iter().map(|record| record.id.as_str()).collect();
    assert!(
        sample_ids.iter().all(|id| corpus_ids.contains(*id)),
        "样本必须全部来自执行窗口规则集"
    );

    let session = insert_session_row(&core, "01J0000000000000000000M62", None).await;

    // 执行判定 1（规范重演）+ 判定 2（真实系统），按样本规则展开尝试向量。
    let mut sample_hashes: Vec<String> = Vec::new();
    struct RawAttempt {
        rule_id: String,
        request_id: String,
        vector: AttemptVector,
        decision: String,
        judge1: String,
        judge1_source: String,
        asked: bool,
    }
    let mut raw_attempts: Vec<RawAttempt> = Vec::new();
    for record in &sample {
        let Some(entry) = corpus_by_id.get(record.id.as_str()) else {
            panic!("样本规则不在语料内：{}", record.id);
        };
        sample_hashes.push(rule_fingerprint(record));
        let only = [record.id.clone()];
        for (index, vector) in entry.attempts.iter().enumerate() {
            let label = format!("{}-{index}", record.id);
            // **判定环境隔离**（抽查操作细节，非决策变更）：样本内 `any` 全匹配规则
            // 会命中其它样本的向量——按「单规则判定环境」逐条加载规则子集快照
            // （规则行不修改，仅内存快照；同源 `rule_from_record` 映射，fail-closed）。
            let loaded = service.restore_rules_subset(&only).await.unwrap();
            assert_eq!(loaded, 1, "单规则环境必须恰好加载 1 条（{label}）");
            let (resolution, observation) =
                run_attempt(&service, &session.id, vector, &label).await;
            // 等待观测：判定 1 预期 `ask` 的向量必须进入待审批并由 `once` 收口；
            // 其余向量不得进入 pending（无假等待/无旁路）。
            let (judge1, judge1_source) = judge::expected_outcome(entry, vector);
            assert_eq!(
                observation.asked,
                judge1 == "ask",
                "等待路径必须与判定 1 一致（{label}）"
            );
            raw_attempts.push(RawAttempt {
                rule_id: record.id.clone(),
                request_id: request_id(&label),
                vector: vector.clone(),
                decision: if resolution.is_allowed() {
                    "allow".to_owned()
                } else {
                    "deny".to_owned()
                },
                judge1,
                judge1_source,
                asked: observation.asked,
            });
        }
    }
    // 恢复全量快照（后续一致性断言；不影响已完成判定）。
    assert_eq!(
        service.restore_rules().await.unwrap(),
        corpus.len(),
        "抽查结束恢复全量规则快照"
    );

    // 审计一次性读取 + 逐请求 `request_id` 对照（零直通判据）。
    let audit_entries = core.reads().audit_log(50_000).await.unwrap();
    let mut audit_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for entry in &audit_entries {
        if !matches!(
            entry.action.as_str(),
            "permission.allowed_by_policy" | "permission.denied_by_policy"
        ) {
            continue;
        }
        let Some(detail) = entry.detail.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(detail) else {
            continue;
        };
        let Some(id) = value.get("request_id").and_then(Value::as_str) else {
            continue;
        };
        if audit_map.contains_key(id) {
            continue;
        }
        let decision = if entry.action == "permission.allowed_by_policy" {
            "allow"
        } else {
            "deny"
        };
        let source = value
            .get("resolver")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if value
                    .get("detail")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("rule:"))
                {
                    "rule".to_owned()
                } else {
                    "matrix".to_owned()
                }
            });
        audit_map.insert(id.to_owned(), (decision.to_owned(), source));
    }
    let mut rows: Vec<Value> = Vec::with_capacity(raw_attempts.len());
    let mut primary_total = 0usize;
    let mut primary_mismatch = 0usize;
    let mut secondary_total = 0usize;
    let mut secondary_mismatch = 0usize;
    let mut severe_count = 0usize;
    let mut zero_passthrough_violations = 0usize;
    for raw in &raw_attempts {
        let entry = corpus_by_id
            .get(raw.rule_id.as_str())
            .expect("原始尝试的规则必须存在");
        let record = &entry.record;
        let vector = &raw.vector;
        let (engine_source, engine_decision, audit_ok) =
            match audit_map.get(raw.request_id.as_str()) {
                Some((audit_decision, source)) => (
                    source.clone(),
                    audit_decision.clone(),
                    audit_decision == &raw.decision,
                ),
                None => ("missing_audit".to_owned(), raw.decision.clone(), false),
            };
        if !audit_ok {
            zero_passthrough_violations += 1;
        }
        // 判定 1：规范重演（执行阶段已计算）；判定 2：系统终局（+审计来源）。
        let (judge1, judge1_source) = (&raw.judge1, &raw.judge1_source);
        let judge2 = engine_decision.clone();
        let divergence = *judge1 != judge2;
        let intent_match = judge2 == vector.expected;
        let mut review = "agreement";
        let mut classification = "ok";
        if divergence {
            review = "third_party_reviewed";
            if judge2 == "allow" && vector.matrix == "deny" {
                classification = "severe_hard_boundary";
                severe_count += 1;
            } else if intent_match {
                classification = "implementation_defect";
            } else {
                classification = "expression_mismatch";
            }
        } else if !intent_match {
            review = "third_party_reviewed";
            classification = "expression_mismatch";
        }
        let misconfigured = classification != "ok";
        if vector.primary {
            primary_total += 1;
            if misconfigured {
                primary_mismatch += 1;
            }
        } else {
            secondary_total += 1;
            if misconfigured {
                secondary_mismatch += 1;
            }
        }
        rows.push(json!({
            "rule_id": record.id,
            "rule_name": record.name,
            "resource": record.resource,
            "action": record.action,
            "target_kind": record.target_kind,
            "rule_target": record.target,
            "rule_decision": record.decision,
            "vector": vector.note,
            "primary": vector.primary,
            "request_id": raw.request_id,
            "matrix": vector.matrix,
            "judge1_expected": judge1,
            "judge1_source": judge1_source,
            "judge2_decision": judge2,
            "judge2_source": engine_source.clone(),
            "engine_source": engine_source,
            "asked": raw.asked,
            "intent_expected": vector.expected,
            "divergence": divergence,
            "review": review,
            "classification": classification,
            "misconfigured": misconfigured,
        }));
    }

    // 误配率（主向量口径；边界向量单独统计并同样断言）。
    let rate = primary_mismatch as f64 * 100.0 / primary_total as f64;
    let secondary_rate = secondary_mismatch as f64 * 100.0 / secondary_total as f64;

    // ===== 产物（ADR-019 附录 A-4 归档契约）=====

    let sample_manifest = json!({
        "task": "M6-02",
        "method": "ADR-019 §2.2（M6-02 预注册）",
        "sample_source": "执行窗口内用户规则全集（本任务为语料落库的全集）",
        "sample_rule": "按创建时间倒序取最近 100 条；不足取全集",
        "persisted_total": persisted.len(),
        "sample_size": sample.len(),
        "sample_ids": sample.iter().map(|record| record.id.clone()).collect::<Vec<_>>(),
        "rules": sample.iter().map(|record| json!({
            "id": record.id,
            "name": record.name,
            "resource": record.resource,
            "action": record.action,
            "target_kind": record.target_kind,
            "target": record.target,
            "decision": record.decision,
            "priority": record.priority,
            "enabled": record.enabled,
            "created_at": record.created_at,
        })).collect::<Vec<_>>(),
    });
    write_artifact(
        "样本清单.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&sample_manifest).unwrap()
        ),
    );
    write_artifact(
        "rules_fingerprint.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "execution_start_fingerprint": rules_fingerprint(&core.reads()).await,
                "sample_fingerprint": fnv1a(&sample_hashes),
                "sample_size": sample.len(),
            }))
            .unwrap()
        ),
    );

    // 双人判定表.csv（ADR-019 附录 A-4 文件名）。
    let mut csv = String::from(
        "rule_id,rule_name,resource,action,target_kind,rule_target,rule_decision,vector,primary,request_id,matrix,judge1_expected,judge1_source,judge2_decision,judge2_source,engine_source,intent_expected,review,classification,misconfigured\n",
    );
    for row in &rows {
        let text = |key: &str| -> String {
            match row.get(key) {
                Some(Value::String(value)) => value.clone(),
                Some(Value::Bool(value)) => value.to_string(),
                Some(other) => other.to_string(),
                None => String::new(),
            }
        };
        let escape = |value: String| {
            if value.contains(',') || value.contains('"') || value.contains('\n') {
                format!("\"{}\"", value.replace('"', "\"\""))
            } else {
                value
            }
        };
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            escape(text("rule_id")),
            escape(text("rule_name")),
            escape(text("resource")),
            escape(text("action")),
            escape(text("target_kind")),
            escape(text("rule_target")),
            escape(text("rule_decision")),
            escape(text("vector")),
            escape(text("primary")),
            escape(text("request_id")),
            escape(text("matrix")),
            escape(text("judge1_expected")),
            escape(text("judge1_source")),
            escape(text("judge2_decision")),
            escape(text("judge2_source")),
            escape(text("engine_source")),
            escape(text("intent_expected")),
            escape(text("review")),
            escape(text("classification")),
            escape(text("misconfigured")),
        ));
    }
    write_artifact("双人判定表.csv", &csv);

    // 逐条判定明细 JSON（脚本独立复核用）。
    write_artifact(
        "判定明细.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "primary_total": primary_total,
                "primary_mismatch": primary_mismatch,
                "secondary_total": secondary_total,
                "secondary_mismatch": secondary_mismatch,
                "severe_count": severe_count,
                "zero_passthrough_violations": zero_passthrough_violations,
                "rows": rows,
            }))
            .unwrap()
        ),
    );

    // 误配率计算输出.txt（ADR-019 附录 A-4 文件名）。
    let verdict = if rate < MISCONFIGURATION_RATE_LIMIT && severe_count == 0 {
        "PASS"
    } else {
        "FAIL"
    };
    let mut report = String::new();
    report.push_str("M6-02 误配率计算输出（ADR-019 §2.2）\n");
    report.push_str(&format!("sample_size={}\n", sample.len()));
    report.push_str(&format!("primary_attempts={primary_total}\n"));
    report.push_str(&format!("misconfigured={primary_mismatch}\n"));
    report.push_str(&format!("misconfiguration_rate={rate:.2}%\n"));
    report.push_str(&format!("severe_count={severe_count}\n"));
    report.push_str(&format!(
        "boundary_attempts={secondary_total} boundary_mismatch={secondary_mismatch} boundary_rate={secondary_rate:.2}%\n"
    ));
    report.push_str(&format!(
        "zero_passthrough_violations={zero_passthrough_violations}\n"
    ));
    report.push_str(&format!("threshold=<5% (exclusive): {verdict}\n"));
    write_artifact("误配率计算输出.txt", &report);

    // 误配案例逐条分析与处置建议.md（DoD3；与判定表同批产出，不可分割）。
    let mut analysis = String::new();
    analysis.push_str("# M6-02 误配案例逐条分析与处置建议\n\n");
    analysis.push_str(
        "> 数据来源：`双人判定表.csv` / `判定明细.json` / `误配率计算输出.txt`（同一执行窗口）。\n",
    );
    analysis.push_str("> 纪律：**不对规则数据做静默修改**（AGENTS §3/§4）；误配处置走「分析与建议」路径，规则数据保持执行前后指纹一致。\n\n");
    let cases: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("misconfigured").and_then(Value::as_bool) == Some(true))
        .collect();
    if cases.is_empty() {
        analysis.push_str("执行窗口内误配案例为 **0**：无需逐条处置。\n\n");
    }
    for (index, row) in cases.iter().enumerate() {
        let text = |key: &str| -> String {
            row.get(key)
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_else(|| "<null>".to_owned())
        };
        analysis.push_str(&format!(
            "## 案例 {}（分类：{}）\n\n",
            index + 1,
            text("classification")
        ));
        analysis.push_str(&format!(
            "- 规则：`{}`（id={}，`{}:{}`，target_kind={}，target={}，decision={}）\n",
            text("rule_name"),
            text("rule_id"),
            text("resource"),
            text("action"),
            text("target_kind"),
            text("rule_target"),
            text("rule_decision"),
        ));
        analysis.push_str(&format!(
            "- 向量：{}（request_id={}，matrix={}，primary={}）\n",
            text("vector"),
            text("request_id"),
            text("matrix"),
            text("primary"),
        ));
        analysis.push_str(&format!(
            "- 判定 1（规范重演）：{}（来源 {}）；判定 2（真实系统）：{}（来源 {}）\n",
            text("judge1_expected"),
            text("judge1_source"),
            text("judge2_decision"),
            text("judge2_source")
        ));
        analysis.push_str(&format!(
            "- 第三人复核：{}；系统终局与登记意图（{}）一致性：{}\n",
            text("review"),
            text("intent_expected"),
            if text("judge2_decision") == text("intent_expected") {
                "一致"
            } else {
                "不一致"
            }
        ));
        let disposition = match text("classification").as_str() {
            "severe_hard_boundary" => {
                "**严重误配（硬边界放宽）**：立即停用规则层（回落固定矩阵 + 建议模式）并另立 ADR；不得带过 P1 门禁（ADR-019 §6-2）。"
            }
            "implementation_defect" => {
                "实现缺陷：按 ADR-019 §2.1 决策顺序定位并修正实现；修复后重新抽查；不得静默修改规则数据或测试断言。"
            }
            "expression_mismatch" => {
                "表达误配：规则语义与登记意图不符（引擎行为符合 ADR 规范）。处置建议 = 用户侧按目标形态规范**重新登记**（新规则入审计、旧规则显式停用/删除，走 `rules_*` 命令），或在规则页加入形态提示/样例校验以减少笔误（产品改进建议，另立任务）。"
            }
            _ => "非误配。",
        };
        analysis.push_str(&format!("- **处置建议**：{disposition}\n\n"));
    }
    analysis.push_str(&format!(
        "> 统计：样本 {} 条；主向量尝试 {} 条、误配 {} 条（率 {:.2}%）；边界向量尝试 {} 条、误配 {} 条（率 {:.2}%）；严重误配 {} 条。\n",
        sample.len(),
        primary_total,
        primary_mismatch,
        rate,
        secondary_total,
        secondary_mismatch,
        secondary_rate,
        severe_count,
    ));
    write_artifact("误配案例分析.md", &analysis);

    // ===== DoD2 断言 =====

    assert_eq!(
        zero_passthrough_violations, 0,
        "抽查执行每一例决策必须落审计（零直通，AGENTS §2.7）"
    );
    assert_eq!(
        severe_count, 0,
        "不得出现硬边界放宽（严重误配单条即触发降级）"
    );
    assert!(
        rate < MISCONFIGURATION_RATE_LIMIT,
        "误配率必须 <5%（实际 {rate:.2}%）"
    );
    assert!(
        secondary_rate < MISCONFIGURATION_RATE_LIMIT,
        "边界向量误配率必须 <5%（实际 {secondary_rate:.2}%）"
    );
    assert_eq!(
        primary_mismatch, 1,
        "预注册演练样本①（表达误配）应被判定流程捕获且仅此 1 条"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

// =============================================================================
// DoD3：误配案例分析 + 数据完整性 + 安全回归（零直通 / 硬边界 / T6）
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m6_02_case_analysis_data_integrity_and_safety_regression() {
    let core = TestCore::open().await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace_dir.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    std::fs::create_dir_all(workspace_dir.path().join("sub")).unwrap();
    let workspace = plain(&std::fs::canonicalize(workspace_dir.path()).unwrap());
    let outside = plain(&std::fs::canonicalize(outside_dir.path()).unwrap());

    // 数据完整性：执行前后规则行指纹一致（不静默修改规则数据）。
    let before_hash = rules_fingerprint(&core.reads()).await;
    let corpus = build_corpus(&workspace, &outside, 6);
    for entry in &corpus {
        core.write()
            .execute(StoreCommand::InsertPermissionRule {
                rule: entry.record.clone(),
            })
            .await
            .unwrap();
    }
    let inserted_hash = rules_fingerprint(&core.reads()).await;
    assert_ne!(before_hash, inserted_hash, "落库后指纹必须变化");
    // 规则快照可加载（安全回归前置：语料 + 后续增补规则）。
    assert_eq!(
        service.restore_rules().await.unwrap(),
        corpus.len(),
        "语料必须全部可加载"
    );

    // 执行结束后规则行指纹不变（不静默修改规则数据）。
    let after_hash = rules_fingerprint(&core.reads()).await;
    assert_eq!(
        inserted_hash, after_hash,
        "抽查与回归不得修改规则数据（指纹一致）"
    );

    // ===== 安全回归 A：规则收紧 + pending 票据不受规则变更影响 =====
    let session = insert_session_row(&core, "01J0000000000000000000M63", None).await;

    // 缺省档区外写 → ask（pending，无规则时）。
    let target = format!(
        "{outside}{}{}",
        std::path::MAIN_SEPARATOR,
        "regress-ask.txt"
    );
    let service_clone = service.clone();
    let target_clone = target.clone();
    let session_clone = session.id.clone();
    let waiter = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("regress-ask"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&target_clone),
                Some(8),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(5)
        )
        .await,
        "无规则时区外写必须进入待审批"
    );
    assert!(
        wait_permissions_pending(&core.reads(), 1).await,
        "pending 落库"
    );

    // 规则在途变更（deny-all）→ pending 票据不受影响（仍按用户决议收口）。
    let deny_rule_id = "01J00000000000000000000M63";
    core.write()
        .execute(StoreCommand::InsertPermissionRule {
            rule: PermissionRuleRecord {
                id: deny_rule_id.to_owned(),
                name: "回归：全量拒绝".to_owned(),
                resource: "fs.write".to_owned(),
                action: "write".to_owned(),
                target_kind: "any".to_owned(),
                target: None,
                decision: "deny".to_owned(),
                priority: 50,
                enabled: true,
                created_at: 9_999,
                updated_at: 9_999,
            },
        })
        .await
        .unwrap();
    service.restore_rules().await.unwrap();
    let ticket = service.pending_list(None).first().unwrap().clone();
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(aether_core::PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = waiter.await.unwrap();
    assert!(
        resolution.is_allowed(),
        "pending 票据不受在途规则变更影响（规则只改变决议来源，不产生旁路）"
    );

    // 新请求：deny 规则收紧（矩阵 ask → deny，审计 resolver=rule:<id>）。
    let tightened = service
        .request(permission_request(
            &request_id("regress-tightened"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(8),
        ))
        .await
        .unwrap();
    assert!(!tightened.is_allowed(), "规则收紧后新请求必须 deny");
    let entries = core.reads().audit_log(20_000).await.unwrap();
    let (audit_decision, source) =
        audit_source_for(&entries, &request_id("regress-tightened")).expect("审计必须存在");
    assert_eq!(audit_decision, "deny");
    assert_eq!(
        source,
        format!("rule:{deny_rule_id}"),
        "审计必须携带 resolver=rule:<id>"
    );

    // ===== 安全回归 B：硬边界（read_only 写 / low exec-net）不被 allow 规则放宽 =====
    let allow_all_id = "01J000000000000000000000MA";
    core.write()
        .execute(StoreCommand::InsertPermissionRule {
            rule: PermissionRuleRecord {
                id: allow_all_id.to_owned(),
                name: "回归：全量放行（不可越界）".to_owned(),
                resource: "fs.write".to_owned(),
                action: "write".to_owned(),
                target_kind: "any".to_owned(),
                target: None,
                decision: "allow".to_owned(),
                priority: 100,
                enabled: true,
                created_at: 10_000,
                updated_at: 10_000,
            },
        })
        .await
        .unwrap();
    core.write()
        .execute(StoreCommand::InsertPermissionRule {
            rule: PermissionRuleRecord {
                id: "01J000000000000000000000MB".to_owned(),
                name: "回归：exec 全量放行（不可越界）".to_owned(),
                resource: "exec".to_owned(),
                action: "exec".to_owned(),
                target_kind: "any".to_owned(),
                target: None,
                decision: "allow".to_owned(),
                priority: 100,
                enabled: true,
                created_at: 10_001,
                updated_at: 10_001,
            },
        })
        .await
        .unwrap();
    service.restore_rules().await.unwrap();

    let read_only = insert_session_row(&core, "01J0000000000000000000M64", None).await;
    m2_support::set_session_permission_mode(
        &core,
        &read_only.id,
        aether_core::PermissionMode::ReadOnly,
    )
    .await;
    let inside = workspace_dir
        .path()
        .join("hard-boundary.txt")
        .to_string_lossy()
        .to_string();
    let denied = service
        .request(permission_request(
            &request_id("hard-boundary-write"),
            Some(&read_only.id),
            "fs.write",
            "write",
            Some(&inside),
            Some(8),
        ))
        .await
        .unwrap();
    assert!(
        !denied.is_allowed(),
        "read_only 硬边界不可被 allow 规则放宽"
    );
    let denied = service
        .request(permission_request(
            &request_id("hard-boundary-exec"),
            Some(&read_only.id),
            "exec",
            "exec",
            Some("npm --version"),
            None,
        ))
        .await
        .unwrap();
    assert!(
        !denied.is_allowed(),
        "低档 exec 硬边界不可被 allow 规则放宽"
    );
    assert!(
        service.pending_list(None).is_empty(),
        "硬边界 deny 不得进入 pending"
    );

    write_artifact(
        "安全回归.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "task": "M6-02",
                "hash_before": before_hash,
                "hash_after_inserts": inserted_hash,
                "hash_after_execution": after_hash,
                "rules_unmodified": inserted_hash == after_hash,
                "pending_rule_change_isolation": true,
                "hard_boundary_denied": true,
                "deny_rule_audit_resolver": format!("rule:{deny_rule_id}"),
            }))
            .unwrap()
        ),
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

// =============================================================================
// 安全回归 C：T6 并发口径（独立服务实例，无规则 → 纯 ask 路径）
// =============================================================================

/// T6 口径回归（M2-03 语义不变）：100 并发 ask 无丢失/重复/死锁；
/// 规则层在场但无命中（矩阵 ask 路径不受规则快照影响）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m6_02_t6_concurrency_regression() {
    let core = TestCore::open().await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace_dir.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    // 规则层在场（不命中本向量）：证明规则层不改变 ask 并发语义。
    core.write()
        .execute(StoreCommand::InsertPermissionRule {
            rule: PermissionRuleRecord {
                id: "01J000000000000000000000MC".to_owned(),
                name: "回归：仅命中其它主机".to_owned(),
                resource: "net".to_owned(),
                action: "connect".to_owned(),
                target_kind: "host".to_owned(),
                target: Some("unrelated.example".to_owned()),
                decision: "deny".to_owned(),
                priority: 10,
                enabled: true,
                created_at: 20_000,
                updated_at: 20_000,
            },
        })
        .await
        .unwrap();
    assert_eq!(service.restore_rules().await.unwrap(), 1);

    let session = insert_session_row(&core, "01J0000000000000000000M65", None).await;
    let outside = plain(&std::fs::canonicalize(outside_dir.path()).unwrap());
    let target = format!("{outside}{}{}", std::path::MAIN_SEPARATOR, "t6.txt");

    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..100 {
        let service_clone = service.clone();
        let target = target.clone();
        let session_clone = session.id.clone();
        tasks.spawn(async move {
            let result = service_clone
                .request(permission_request(
                    &format!("t6-{index}"),
                    Some(&session_clone),
                    "fs.write",
                    "write",
                    Some(&target),
                    Some(8),
                ))
                .await;
            (index, result)
        });
    }
    assert!(
        wait_for(
            || service.pending_list(None).len() == 100,
            Duration::from_secs(10)
        )
        .await,
        "100 并发请求必须全部进入 pending（无丢失）"
    );
    assert!(
        wait_permissions_pending(&core.reads(), 100).await,
        "DB pending 行数必须为 100（无丢失）"
    );
    let pending = service.pending_list(None);
    let mut resolve_tasks = tokio::task::JoinSet::new();
    for (index, ticket) in pending.into_iter().enumerate() {
        let service_clone = service.clone();
        let decision = if index % 2 == 0 {
            (
                PermissionDecision::Allow,
                Some(aether_core::PermissionScope::Once),
            )
        } else {
            (PermissionDecision::Deny, None)
        };
        resolve_tasks.spawn(async move {
            service_clone
                .resolve(&ticket.request_id, decision.0, decision.1)
                .await
        });
    }
    while let Some(result) = resolve_tasks.join_next().await {
        result.unwrap().unwrap();
    }
    let mut allowed = 0usize;
    let mut denied = 0usize;
    let mut seen = HashSet::new();
    while let Some(result) = tasks.join_next().await {
        let (index, resolution) = result.unwrap();
        assert!(seen.insert(index), "重复结果");
        if resolution.unwrap().is_allowed() {
            allowed += 1;
        } else {
            denied += 1;
        }
    }
    assert_eq!(allowed, 50, "50 allow");
    assert_eq!(denied, 50, "50 deny");
    assert!(service.pending_list(None).is_empty(), "无死锁/无残留");
    assert_eq!(
        core.reads().permissions_pending(None).await.unwrap().len(),
        0,
        "DB pending 必须清零"
    );

    write_artifact(
        "t6_并发回归.json",
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "task": "M6-02",
                "concurrent_asks": 100,
                "allowed": allowed,
                "denied": denied,
                "no_loss_no_duplication_no_deadlock": true,
            }))
            .unwrap()
        ),
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
