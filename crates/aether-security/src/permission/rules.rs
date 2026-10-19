//! 用户自定义权限规则模型（M6-01；ADR-019 附录 A-1）——纯决策层（无持久化）。
//!
//! 规则 = `(name, resource, action, target_kind, target, decision, priority, enabled)`；
//! 规则**不产生 `ask`**（`ask` 仅由档位矩阵产生）。决策顺序（ADR-019 §2.1）：
//! ① 档位矩阵为硬边界（本模块不参与）；② 矩阵 `allow` → 任一命中 `deny` 规则收紧；
//! ③ 矩阵 `ask` → `deny` 优先，否则命中 `allow` 免逐次审批；④ 顶档矩阵全放行后规则
//! 仅可收紧。冲突：同决定内 `priority` 高者先（仅影响审计呈现的命中顺序）。
//!
//! 持久化、审计与命令面在 `aether-control` / `aether-tauri`（`permission_rules` 表）。

use std::path::Path;

use super::path::{canonicalize_free_path, case_insensitive_platform};
use super::policy::PermissionResource;

/// 规则目标类型（`permission_rules.target_kind` CHECK 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleTargetKind {
    Path,
    Command,
    Host,
    Any,
}

impl RuleTargetKind {
    pub const ALL: [Self; 4] = [Self::Path, Self::Command, Self::Host, Self::Any];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Command => "command",
            Self::Host => "host",
            Self::Any => "any",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == code)
    }
}

/// 规则决议（`permission_rules.decision` CHECK 枚举；规则不产生 `ask`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleDecision {
    Allow,
    Deny,
}

impl RuleDecision {
    pub const ALL: [Self; 2] = [Self::Allow, Self::Deny];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|decision| decision.as_str() == code)
    }
}

/// 用户自定义权限规则（模型侧投影；持久化行见 `aether-store`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub id: String,
    pub name: String,
    pub resource: PermissionResource,
    pub action: String,
    pub target_kind: RuleTargetKind,
    /// 归一化目标形态（`path` = canonical 形态；`any` 为 `None`）。
    pub target: Option<String>,
    pub decision: RuleDecision,
    /// 0–100；高者先（仅审计呈现顺序）。
    pub priority: i64,
    pub enabled: bool,
    pub created_at: i64,
}

/// 规则校验失败（DTO/命令层映射为 `invalid_value` / `invalid_enum` 等结构化错误）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    /// `action` 与 `resource` 不匹配（兼容矩阵：`fs.read→read` 等）。
    ResourceActionMismatch {
        resource: &'static str,
        expected_action: &'static str,
        action: String,
    },
    /// `target_kind` 与 `resource` 不兼容（ADR-019 附录 A-1 硬口径）。
    TargetKindMismatch {
        resource: &'static str,
        target_kind: &'static str,
    },
    /// 目标形态非法（空目标、超长、字符集、canonicalize 失败等）。
    InvalidTargetForm { reason: String },
}

impl RuleError {
    /// 稳定短语（测试与 DTO 层共用）。
    pub fn message(&self) -> String {
        match self {
            Self::ResourceActionMismatch {
                resource,
                expected_action,
                action,
            } => format!("resource={resource} 要求 action={expected_action}，收到 {action}"),
            Self::TargetKindMismatch {
                resource,
                target_kind,
            } => format!("resource={resource} 不接受 target_kind={target_kind}"),
            Self::InvalidTargetForm { reason } => format!("规则目标形态非法：{reason}"),
        }
    }
}

/// 命令类目标长度上限（ADR-018 附录 A-1：`command` 类 ≤256 字符）。
pub const RULE_COMMAND_MAX_LEN: usize = 256;

/// 主机名类目标长度上限（DNS 全限定名上限）。
pub const RULE_HOST_MAX_LEN: usize = 253;

/// 校验并归一化规则目标（ADR-019 附录 A-1 兼容矩阵）。
///
/// - `fs.read` / `fs.write`：`path` / `any`；`path` 做 canonicalize 形态校验
///   （不做存在性），原始输入以分隔符收尾 → 目录前缀形态（canonical + 分隔符）；
/// - `exec`：`command` / `any`（可执行名，无路径分隔符/空白）；
/// - `net`：`host` / `any`（精确主机名或 `.` 前缀后缀；统一小写）；
/// - `any` 目标仅作占位（`x` 等非空串），不参与匹配（全匹配）。
pub fn normalize_rule_target(
    resource: PermissionResource,
    target_kind: RuleTargetKind,
    target: Option<&str>,
) -> Result<Option<String>, RuleError> {
    let kind_ok = match resource {
        PermissionResource::FsRead | PermissionResource::FsWrite => {
            matches!(target_kind, RuleTargetKind::Path | RuleTargetKind::Any)
        }
        PermissionResource::Exec => {
            matches!(target_kind, RuleTargetKind::Command | RuleTargetKind::Any)
        }
        PermissionResource::Net => {
            matches!(target_kind, RuleTargetKind::Host | RuleTargetKind::Any)
        }
    };
    if !kind_ok {
        return Err(RuleError::TargetKindMismatch {
            resource: resource.as_str(),
            target_kind: target_kind.as_str(),
        });
    }
    match target_kind {
        RuleTargetKind::Any => Ok(None),
        RuleTargetKind::Path => {
            let raw = target.unwrap_or("").trim();
            if raw.is_empty() {
                return Err(RuleError::InvalidTargetForm {
                    reason: "path 类规则目标不得为空".to_owned(),
                });
            }
            let directory_prefix = raw.ends_with('/') || raw.ends_with('\\');
            let canonical =
                canonicalize_free_path(raw).map_err(|violation| RuleError::InvalidTargetForm {
                    reason: violation.reason(),
                })?;
            let mut text = canonical.to_string_lossy().to_string();
            if directory_prefix {
                if !text.ends_with(std::path::MAIN_SEPARATOR) {
                    text.push(std::path::MAIN_SEPARATOR);
                }
            } else {
                text = text.trim_end_matches(std::path::MAIN_SEPARATOR).to_owned();
            }
            Ok(Some(text))
        }
        RuleTargetKind::Command => {
            let raw = target.unwrap_or("").trim();
            if raw.is_empty() {
                return Err(RuleError::InvalidTargetForm {
                    reason: "command 类规则目标不得为空".to_owned(),
                });
            }
            if raw.chars().count() > RULE_COMMAND_MAX_LEN {
                return Err(RuleError::InvalidTargetForm {
                    reason: format!("command 类规则目标超长（>{RULE_COMMAND_MAX_LEN}）"),
                });
            }
            if raw.contains('/')
                || raw.contains('\\')
                || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            {
                return Err(RuleError::InvalidTargetForm {
                    reason: "command 类规则目标须为可执行名（不含路径分隔符/空白/控制字符）"
                        .to_owned(),
                });
            }
            Ok(Some(raw.to_owned()))
        }
        RuleTargetKind::Host => {
            let raw = target.unwrap_or("").trim().to_ascii_lowercase();
            if raw.is_empty() {
                return Err(RuleError::InvalidTargetForm {
                    reason: "host 类规则目标不得为空".to_owned(),
                });
            }
            if raw.chars().count() > RULE_HOST_MAX_LEN {
                return Err(RuleError::InvalidTargetForm {
                    reason: format!("host 类规则目标超长（>{RULE_HOST_MAX_LEN}）"),
                });
            }
            let suffix_form = raw.starts_with('.');
            let name = raw.strip_prefix('.').unwrap_or(&raw);
            let labels: Vec<&str> = name.split('.').collect();
            if labels.is_empty()
                || labels
                    .iter()
                    .any(|label| label.is_empty() || !valid_host_label(label))
            {
                return Err(RuleError::InvalidTargetForm {
                    reason: "host 类规则目标须为合法主机名（或 `.` 前缀后缀）".to_owned(),
                });
            }
            if !suffix_form && labels.len() == 1 {
                // 单标签主机名（localhost 等）允许。
            }
            Ok(Some(raw))
        }
    }
}

fn valid_host_label(label: &str) -> bool {
    label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

/// 规则命中记录（审计呈现用；调用方按序写入 `resolver=rule:<id>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHit {
    pub id: String,
    pub name: String,
    pub decision: RuleDecision,
}

/// 规则层评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleOutcome {
    /// 命中 deny（deny 优先；矩阵 `ask` 与 `allow` 均被收紧）。
    Deny {
        hits: Vec<RuleHit>,
    },
    /// 命中 allow（仅矩阵 `ask` 场景由调用方采用免审批）。
    Allow {
        hits: Vec<RuleHit>,
    },
    NoMatch,
}

impl RuleOutcome {
    pub fn hits(&self) -> &[RuleHit] {
        match self {
            Self::Deny { hits } | Self::Allow { hits } => hits,
            Self::NoMatch => &[],
        }
    }
}

/// 评估规则层（纯函数；决策顺序见模块文档）。
///
/// `canonical_target`：路径类请求的 canonical 目标（`PolicyVerdict.canonical_target`）；
/// 非路径请求为 `None`（`path` 规则不命中）。
pub fn evaluate_rules(
    rules: &[PermissionRule],
    resource: PermissionResource,
    action: &str,
    target: Option<&str>,
    canonical_target: Option<&Path>,
) -> RuleOutcome {
    let mut deny_hits = Vec::new();
    let mut allow_hits = Vec::new();
    for rule in rules.iter().filter(|rule| rule.enabled) {
        if rule.resource != resource || rule.action != action {
            continue;
        }
        if !rule_matches(rule, target, canonical_target) {
            continue;
        }
        let hit = RuleHit {
            id: rule.id.clone(),
            name: rule.name.clone(),
            decision: rule.decision,
        };
        match rule.decision {
            RuleDecision::Deny => deny_hits.push((rule, hit)),
            RuleDecision::Allow => allow_hits.push((rule, hit)),
        }
    }
    if !deny_hits.is_empty() {
        return RuleOutcome::Deny {
            hits: order_hits(deny_hits),
        };
    }
    if !allow_hits.is_empty() {
        return RuleOutcome::Allow {
            hits: order_hits(allow_hits),
        };
    }
    RuleOutcome::NoMatch
}

fn order_hits(mut hits: Vec<(&PermissionRule, RuleHit)>) -> Vec<RuleHit> {
    hits.sort_by(|left, right| {
        right
            .0
            .priority
            .cmp(&left.0.priority)
            .then_with(|| left.0.created_at.cmp(&right.0.created_at))
            .then_with(|| left.0.id.cmp(&right.0.id))
    });
    hits.into_iter().map(|(_, hit)| hit).collect()
}

fn rule_matches(
    rule: &PermissionRule,
    target: Option<&str>,
    canonical_target: Option<&Path>,
) -> bool {
    match rule.target_kind {
        RuleTargetKind::Any => true,
        RuleTargetKind::Path => {
            if !rule.resource.is_fs() {
                return false;
            }
            match (rule.target.as_deref(), canonical_target) {
                (Some(rule_target), Some(canonical)) => path_matches(rule_target, canonical),
                _ => false,
            }
        }
        RuleTargetKind::Command => {
            if rule.resource != PermissionResource::Exec {
                return false;
            }
            match (rule.target.as_deref(), target) {
                (Some(rule_target), Some(request_target)) => {
                    command_matches(rule_target, request_target)
                }
                _ => false,
            }
        }
        RuleTargetKind::Host => {
            if rule.resource != PermissionResource::Net {
                return false;
            }
            match (rule.target.as_deref(), target) {
                (Some(rule_target), Some(request_target)) => {
                    host_matches(rule_target, request_target)
                }
                _ => false,
            }
        }
    }
}

/// 路径规则匹配：目录前缀（规则目标以分隔符收尾 → canonical 位于其下）或精确路径。
///
/// 形态鲁棒性（M6-02 抽查缺陷修复）：规则目标与 canonical 目标可能分别为
/// 普通形态与 `\\?\` 扩展形态（Windows `std::fs::canonicalize` 返回扩展形态，
/// 而 `canonicalize_free_path` 剥离后再校验）；比较前统一剥离扩展前缀，
/// 前缀比较按平台大小写规则（Windows 不敏感）。
fn path_matches(rule_target: &str, canonical: &Path) -> bool {
    let canonical = strip_extended(&canonical.to_string_lossy()).to_owned();
    if rule_target.ends_with(std::path::MAIN_SEPARATOR) {
        prefix_matches(rule_target, &canonical)
    } else {
        path_text_eq(rule_target, &canonical)
    }
}

/// 目录前缀比较：剥离扩展前缀 + 按平台大小写规则。
fn prefix_matches(rule_target: &str, canonical: &str) -> bool {
    let rule_target = strip_extended(rule_target);
    if case_insensitive_platform() {
        canonical.len() >= rule_target.len()
            && canonical[..rule_target.len()].eq_ignore_ascii_case(rule_target)
    } else {
        canonical.starts_with(rule_target)
    }
}

/// 路径文本比较（Windows/macOS 大小写不敏感；剥离 `\\?\` 扩展前缀）。
fn path_text_eq(left: &str, right: &str) -> bool {
    let left = strip_extended(left);
    let right = strip_extended(right);
    if case_insensitive_platform() {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn strip_extended(value: &str) -> &str {
    value
        .strip_prefix(r"\\?\")
        .or_else(|| value.strip_prefix("//?/"))
        .unwrap_or(value)
}

/// 命令规则匹配：可执行名精确匹配（参数不参与；词边界 = 空白分隔首词的文件名）。
fn command_matches(rule_target: &str, request_target: &str) -> bool {
    let first = request_target.split_whitespace().next().unwrap_or("");
    let name = Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);
    if case_insensitive_platform() {
        name.eq_ignore_ascii_case(rule_target)
    } else {
        name == rule_target
    }
}

/// 主机规则匹配：精确主机名或 `.` 前缀后缀（标签边界；`example.com` 不命中
/// `evil-example.com`）。
fn host_matches(rule_target: &str, request_host: &str) -> bool {
    let host = request_host.trim().to_ascii_lowercase();
    if let Some(suffix) = rule_target.strip_prefix('.') {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else {
        host == rule_target
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn rule(
        id: &str,
        resource: PermissionResource,
        target_kind: RuleTargetKind,
        target: Option<&str>,
        decision: RuleDecision,
        priority: i64,
    ) -> PermissionRule {
        PermissionRule {
            id: id.to_owned(),
            name: format!("rule-{id}"),
            resource,
            action: resource.rule_action().to_owned(),
            target_kind,
            target: target.map(str::to_owned),
            decision,
            priority,
            enabled: true,
            created_at: 1,
        }
    }

    #[test]
    fn resource_target_kind_compat_matrix_is_enforced() {
        // fs.* × command/host → 拒绝。
        assert!(matches!(
            normalize_rule_target(
                PermissionResource::FsWrite,
                RuleTargetKind::Command,
                Some("rm")
            ),
            Err(RuleError::TargetKindMismatch { .. })
        ));
        assert!(matches!(
            normalize_rule_target(PermissionResource::Exec, RuleTargetKind::Path, Some("C:/x")),
            Err(RuleError::TargetKindMismatch { .. })
        ));
        assert!(matches!(
            normalize_rule_target(
                PermissionResource::Net,
                RuleTargetKind::Command,
                Some("curl")
            ),
            Err(RuleError::TargetKindMismatch { .. })
        ));
        // 合法组合。
        assert!(normalize_rule_target(
            PermissionResource::FsRead,
            RuleTargetKind::Any,
            Some("ignored")
        )
        .unwrap()
        .is_none());
        assert_eq!(
            normalize_rule_target(
                PermissionResource::Exec,
                RuleTargetKind::Command,
                Some("rm")
            )
            .unwrap()
            .as_deref(),
            Some("rm")
        );
        assert_eq!(
            normalize_rule_target(
                PermissionResource::Net,
                RuleTargetKind::Host,
                Some("Example.COM")
            )
            .unwrap()
            .as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn invalid_target_forms_are_rejected() {
        assert!(
            normalize_rule_target(PermissionResource::Exec, RuleTargetKind::Command, Some(""))
                .is_err()
        );
        assert!(normalize_rule_target(
            PermissionResource::Exec,
            RuleTargetKind::Command,
            Some("rm -rf /")
        )
        .is_err());
        assert!(normalize_rule_target(
            PermissionResource::Net,
            RuleTargetKind::Host,
            Some("http://example.com")
        )
        .is_err());
        assert!(normalize_rule_target(
            PermissionResource::Net,
            RuleTargetKind::Host,
            Some("bad-.com")
        )
        .is_err());
        assert!(normalize_rule_target(
            PermissionResource::Net,
            RuleTargetKind::Host,
            Some("exa mple.com")
        )
        .is_err());
        assert!(normalize_rule_target(
            PermissionResource::FsRead,
            RuleTargetKind::Path,
            Some("\\\\server\\share\\x")
        )
        .is_err());
        // 系统生成的 canonical 形态幂等接受（更新旧规则的再规范化路径）。
        // Windows 系统形态为 `\\?\` 扩展前缀 + 盘符；盘符语义仅 Windows 成立，
        // 非 Windows 以同前缀的 POSIX 绝对路径验证剥离后的接受路径。
        #[cfg(windows)]
        assert!(normalize_rule_target(
            PermissionResource::FsRead,
            RuleTargetKind::Path,
            Some("\\\\?\\C:\\x")
        )
        .is_ok());
        #[cfg(not(windows))]
        assert!(normalize_rule_target(
            PermissionResource::FsRead,
            RuleTargetKind::Path,
            Some("//?//tmp/aether-extended-form")
        )
        .is_ok());
    }

    #[test]
    fn deny_wins_over_allow_and_priority_orders_hits() {
        let rules = vec![
            rule(
                "a",
                PermissionResource::FsWrite,
                RuleTargetKind::Any,
                None,
                RuleDecision::Allow,
                5,
            ),
            rule(
                "b",
                PermissionResource::FsWrite,
                RuleTargetKind::Any,
                None,
                RuleDecision::Deny,
                1,
            ),
            rule(
                "c",
                PermissionResource::FsWrite,
                RuleTargetKind::Any,
                None,
                RuleDecision::Deny,
                9,
            ),
        ];
        let outcome = evaluate_rules(
            &rules,
            PermissionResource::FsWrite,
            "write",
            Some("C:/ws/a.txt"),
            None,
        );
        match outcome {
            RuleOutcome::Deny { hits } => {
                assert_eq!(hits.len(), 2);
                assert_eq!(hits[0].id, "c", "priority 高者先（审计顺序）");
                assert_eq!(hits[1].id, "b");
            }
            other => panic!("deny 优先: {other:?}"),
        }
    }

    #[test]
    fn disabled_and_mismatched_rules_do_not_match() {
        let mut disabled = rule(
            "d",
            PermissionResource::FsWrite,
            RuleTargetKind::Any,
            None,
            RuleDecision::Deny,
            0,
        );
        disabled.enabled = false;
        let mismatched = rule(
            "e",
            PermissionResource::FsRead,
            RuleTargetKind::Any,
            None,
            RuleDecision::Deny,
            0,
        );
        let outcome = evaluate_rules(
            &[disabled, mismatched],
            PermissionResource::FsWrite,
            "write",
            Some("C:/ws/a.txt"),
            None,
        );
        assert_eq!(outcome, RuleOutcome::NoMatch);
    }

    #[test]
    fn path_rules_match_exact_and_directory_prefix() {
        let sep = std::path::MAIN_SEPARATOR;
        let exact = rule(
            "p1",
            PermissionResource::FsWrite,
            RuleTargetKind::Path,
            Some(&format!("C:{sep}data{sep}secret.txt")),
            RuleDecision::Deny,
            0,
        );
        let prefix = rule(
            "p2",
            PermissionResource::FsWrite,
            RuleTargetKind::Path,
            Some(&format!("C:{sep}data{sep}private{sep}")),
            RuleDecision::Deny,
            0,
        );
        let rules = [exact, prefix];
        assert!(matches!(
            evaluate_rules(
                &rules,
                PermissionResource::FsWrite,
                "write",
                Some("x"),
                Some(Path::new(&format!("C:{sep}data{sep}secret.txt")))
            ),
            RuleOutcome::Deny { .. }
        ));
        assert!(matches!(
            evaluate_rules(
                &rules,
                PermissionResource::FsWrite,
                "write",
                Some("x"),
                Some(Path::new(&format!(
                    "C:{sep}data{sep}private{sep}sub{sep}x.txt"
                )))
            ),
            RuleOutcome::Deny { .. }
        ));
        assert_eq!(
            evaluate_rules(
                &rules,
                PermissionResource::FsWrite,
                "write",
                Some("x"),
                Some(Path::new(&format!("C:{sep}data{sep}privatex{sep}x.txt")))
            ),
            RuleOutcome::NoMatch,
            "目录前缀按分隔符边界（privatex 不命中 private/）"
        );
        assert_eq!(
            evaluate_rules(
                &rules,
                PermissionResource::FsRead,
                "read",
                Some("x"),
                Some(Path::new(&format!("C:{sep}data{sep}secret.txt")))
            ),
            RuleOutcome::NoMatch,
            "resource 不匹配不命中"
        );
    }

    #[test]
    fn path_directory_prefix_matches_extended_canonical_form() {
        // M6-02 抽查缺陷修复回归：Windows `std::fs::canonicalize` 返回 `\\?\` 扩展形态，
        // 目录前缀规则必须仍然命中（剥离扩展前缀后比较）；精确路径同口径幂等。
        let sep = std::path::MAIN_SEPARATOR;
        let prefix = rule(
            "p3",
            PermissionResource::FsWrite,
            RuleTargetKind::Path,
            Some(&format!("C:{sep}data{sep}private{sep}")),
            RuleDecision::Deny,
            0,
        );
        assert!(matches!(
            evaluate_rules(
                &[prefix],
                PermissionResource::FsWrite,
                "write",
                Some("x"),
                Some(Path::new(&format!(
                    "\\\\?\\C:{sep}data{sep}private{sep}sub{sep}x.txt"
                )))
            ),
            RuleOutcome::Deny { .. }
        ));
        let exact = rule(
            "p4",
            PermissionResource::FsWrite,
            RuleTargetKind::Path,
            Some(&format!("C:{sep}data{sep}secret.txt")),
            RuleDecision::Deny,
            0,
        );
        assert!(matches!(
            evaluate_rules(
                &[exact],
                PermissionResource::FsWrite,
                "write",
                Some("x"),
                Some(Path::new(&format!("\\\\?\\C:{sep}data{sep}secret.txt")))
            ),
            RuleOutcome::Deny { .. }
        ));
    }

    #[test]
    fn command_rules_match_name_with_word_boundary() {
        let rules = [rule(
            "c1",
            PermissionResource::Exec,
            RuleTargetKind::Command,
            Some("rm"),
            RuleDecision::Deny,
            0,
        )];
        // 参数不参与；词边界（rmdir 不命中 rm）。
        assert!(matches!(
            evaluate_rules(
                &rules,
                PermissionResource::Exec,
                "exec",
                Some("rm -rf /tmp/x"),
                None
            ),
            RuleOutcome::Deny { .. }
        ));
        assert_eq!(
            evaluate_rules(
                &rules,
                PermissionResource::Exec,
                "exec",
                Some("rmdir /tmp/x"),
                None
            ),
            RuleOutcome::NoMatch
        );
        // 带路径前缀的可执行名按文件名匹配。
        assert!(matches!(
            evaluate_rules(
                &rules,
                PermissionResource::Exec,
                "exec",
                Some("/usr/bin/rm file.txt"),
                None
            ),
            RuleOutcome::Deny { .. }
        ));
    }

    #[test]
    fn host_rules_match_label_boundary() {
        let rules = [rule(
            "h1",
            PermissionResource::Net,
            RuleTargetKind::Host,
            Some("example.com"),
            RuleDecision::Allow,
            0,
        )];
        assert!(matches!(
            evaluate_rules(
                &rules,
                PermissionResource::Net,
                "connect",
                Some("example.com"),
                None
            ),
            RuleOutcome::Allow { .. }
        ));
        assert_eq!(
            evaluate_rules(
                &rules,
                PermissionResource::Net,
                "connect",
                Some("evil-example.com"),
                None
            ),
            RuleOutcome::NoMatch
        );
        let suffix = [rule(
            "h2",
            PermissionResource::Net,
            RuleTargetKind::Host,
            Some(".example.com"),
            RuleDecision::Deny,
            0,
        )];
        assert!(matches!(
            evaluate_rules(
                &suffix,
                PermissionResource::Net,
                "connect",
                Some("api.example.com"),
                None
            ),
            RuleOutcome::Deny { .. }
        ));
        assert_eq!(
            evaluate_rules(
                &suffix,
                PermissionResource::Net,
                "connect",
                Some("example.com.evil.io"),
                None
            ),
            RuleOutcome::NoMatch
        );
        assert!(matches!(
            evaluate_rules(
                &suffix,
                PermissionResource::Net,
                "connect",
                Some("example.com"),
                None
            ),
            RuleOutcome::Deny { .. }
        ));
    }
}
