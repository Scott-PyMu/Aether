//! 能力路由（M7-02/ADR-022；CP-02 P1 部分、RA-04）。
//!
//! 口径（ADR-022 §2.1）：
//! - `session_create.runtime_id` 缺省 = **自动路由**；显式给出 = **手动优先**
//!   （不参与打分；命令层校验，不经本模块）；
//! - **候选过滤（硬条件）**：① `runtimes.enabled=1`（ADR-015 偏好）且状态 ∈
//!   `ready`/`cold`（`starting`/`degraded`/`disabled` 排除；`cold` 允许入选并触发
//!   预热）；② 请求显式 `model` 时，候选须能路由该模型（存在启用 provider 提供
//!   该模型，或候选声明直通——DSH 按会话选模型，ADR-024；能力项如 `thinking_depth`
//!   **不构成过滤条件**，ADR-010 口径）；
//! - **排序**：`ready` 优先于 `cold` → 固定内置优先级常量（升序，小者先）→
//!   `id` 字典序（相等时）；
//! - **无候选**：结构化失败 [`RouteFailure`]（命令层映射 `no_candidate_runtime`，
//!   message 区分「全部禁用」「无匹配模型」「无 ready 候选」）；不静默回退。
//!
//! 路由决策（[`RouteDecision`]）由调用方（`aether-tauri` 命令层）随会话创建传入控制层，
//! 落审计 `route.decided`（actor=system；ADR-022 附录 A-3）。

use aether_core::RuntimeStatus;

/// 固定路由优先级常量（ADR-022 附录 A-4；不扩展 ADR-015 注册清单 schema）。
pub const ROUTE_PRIORITY_CLAUDE_CODE: u32 = 10;
pub const ROUTE_PRIORITY_CODEX: u32 = 20;
pub const ROUTE_PRIORITY_DEEPSEEK_HARNESS: u32 = 30;
/// `mock` 与未登记 id 同级（仅测试构建使用 `mock`）。
pub const ROUTE_PRIORITY_DEFAULT: u32 = 100;

/// 运行时 id → 固定优先级（升序，小者先；常量级调参记录任务证据，AGENTS §3）。
pub fn route_priority(runtime_id: &str) -> u32 {
    match runtime_id {
        "claude-code" => ROUTE_PRIORITY_CLAUDE_CODE,
        "codex" => ROUTE_PRIORITY_CODEX,
        "deepseek-harness" => ROUTE_PRIORITY_DEEPSEEK_HARNESS,
        _ => ROUTE_PRIORITY_DEFAULT,
    }
}

/// 路由候选（命令层快照；`enabled` 取 `runtimes.enabled` 持久化偏好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCandidate {
    pub id: String,
    /// `runtimes.enabled`（ADR-015 偏好；缺省回退构建期清单值）。
    pub enabled: bool,
    /// 监督器当前状态（D5）。
    pub status: RuntimeStatus,
    /// 该候选是否对请求模型直通（DSH 按会话选模型；M7-06 登记实测映射前恒 `false`）。
    pub model_passthrough: bool,
}

/// 候选追踪（决策 detail；审计 `route.decided` 载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCandidateTrace {
    pub id: String,
    pub enabled: bool,
    pub status: RuntimeStatus,
    pub priority: u32,
    /// 是否通过全部硬条件（进入排序集合）。
    pub eligible: bool,
    /// 参与/排除原因（诊断与审计文案）。
    pub detail: String,
}

/// 路由失败（命令层映射 `no_candidate_runtime`；message 区分三类原因）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteFailure {
    /// 无启用候选（未注册任何运行时，或全部 `enabled=0` / `disabled`）。
    AllDisabled,    /// 有启用候选，但无一能路由请求模型。
    ModelUnavailable { model: String },
    /// 有启用且模型可路由的候选，但状态均不可参与（`starting`/`degraded`）。
    NoReadyCandidate,
}

impl RouteFailure {
    /// 面向用户的原因文本（UI-UX §5 `no_candidate_runtime` 文案「没有可用的运行时：{原因}」）。
    pub fn reason(&self) -> String {
        match self {
            Self::AllDisabled => "未注册任何运行时或全部已禁用".to_owned(),
            Self::ModelUnavailable { model } => {
                format!("没有运行时提供该模型（{model}）")
            }
            Self::NoReadyCandidate => {
                "候选均不可用（starting/degraded；无 ready/cold 候选）".to_owned()
            }
        }
    }
}

/// 路由决策（自动路由成功；`session.created` 回显 `routed=true` + 审计 `route.decided`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    /// 选中的运行时 id。
    pub runtime_id: String,
    /// 选中候选的优先级常量。
    pub priority: u32,
    /// 决策原因（候选数/选中/状态）。
    pub reason: String,
    /// 全候选追踪（含被排除项与原因）。
    pub candidates: Vec<RouteCandidateTrace>,
}

/// 自动路由（ADR-022 §2.1#2/#3/#4）。
///
/// `model_available` = 存在启用 provider 提供请求模型（命令层查询 `providers`/
/// `provider_models` 的快照；`model = None` 时不参与判定）。
pub fn route_runtime(
    model: Option<&str>,
    model_available: bool,
    candidates: &[RouteCandidate],
) -> Result<RouteDecision, RouteFailure> {
    let mut traces: Vec<RouteCandidateTrace> = Vec::with_capacity(candidates.len());
    let mut enabled_count = 0usize;
    let mut model_eligible_count = 0usize;

    for candidate in candidates {
        let priority = route_priority(&candidate.id);
        let mut reasons: Vec<String> = Vec::new();
        if !candidate.enabled || candidate.status == RuntimeStatus::Disabled {
            reasons.push(if candidate.status == RuntimeStatus::Disabled {
                "status=disabled".to_owned()
            } else {
                "enabled=0（用户禁用偏好）".to_owned()
            });
        }
        let model_ok = if reasons.is_empty() {
            enabled_count += 1;
            if model.is_none() || model_available || candidate.model_passthrough {
                true
            } else {
                let model = model.unwrap_or_default();
                reasons.push(format!("无启用供应商提供该模型（{model}）"));
                false
            }
        } else {
            false
        };
        if model_ok {
            model_eligible_count += 1;
        }
        let status_ok = matches!(
            candidate.status,
            RuntimeStatus::Ready | RuntimeStatus::Cold
        );
        if model_ok && !status_ok {
            reasons.push(format!("status={}（不可参与路由）", candidate.status.as_str()));
        }
        let eligible = model_ok && status_ok;
        let detail = if eligible {
            format!(
                "参与：status={}，优先级={priority}{}",
                candidate.status.as_str(),
                if candidate.status == RuntimeStatus::Cold {
                    "（cold 入选并触发预热）"
                } else {
                    ""
                }
            )
        } else {
            format!("排除：{}", reasons.join("；"))
        };
        traces.push(RouteCandidateTrace {
            id: candidate.id.clone(),
            enabled: candidate.enabled,
            status: candidate.status,
            priority,
            eligible,
            detail,
        });
    }

    let mut eligible: Vec<&RouteCandidateTrace> = traces.iter().filter(|t| t.eligible).collect();
    if eligible.is_empty() {
        let failure = if enabled_count == 0 {
            RouteFailure::AllDisabled
        } else if model_eligible_count == 0 {
            RouteFailure::ModelUnavailable {
                model: model.unwrap_or_default().to_owned(),
            }
        } else {
            RouteFailure::NoReadyCandidate
        };
        return Err(failure);
    }

    // 排序：ready 优先于 cold → 优先级升序 → id 字典序。
    eligible.sort_by(|left, right| {
        let ready_rank = |trace: &RouteCandidateTrace| usize::from(trace.status == RuntimeStatus::Cold);
        ready_rank(left)
            .cmp(&ready_rank(right))
            .then(left.priority.cmp(&right.priority))
            .then_with(|| left.id.cmp(&right.id))
    });
    let selected = eligible
        .first()
        .copied()
        .ok_or(RouteFailure::NoReadyCandidate)?;

    Ok(RouteDecision {
        runtime_id: selected.id.clone(),
        priority: selected.priority,
        reason: format!(
            "自动路由：候选 {} 个（启用 {} 个），选中 {}（status={}，优先级={}）",
            candidates.len(),
            enabled_count,
            selected.id,
            selected.status.as_str(),
            selected.priority
        ),
        candidates: traces,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn candidate(id: &str, status: RuntimeStatus) -> RouteCandidate {
        RouteCandidate {
            id: id.to_owned(),
            enabled: true,
            status,
            model_passthrough: false,
        }
    }

    fn disabled(id: &str, status: RuntimeStatus) -> RouteCandidate {
        RouteCandidate {
            id: id.to_owned(),
            enabled: false,
            status,
            model_passthrough: false,
        }
    }

    #[test]
    fn priorities_follow_adr_fixed_constants() {
        assert_eq!(route_priority("claude-code"), 10);
        assert_eq!(route_priority("codex"), 20);
        assert_eq!(route_priority("deepseek-harness"), 30);
        assert_eq!(route_priority("mock"), 100);
        assert_eq!(route_priority("unknown-runtime"), 100);
    }

    #[test]
    fn fixed_priority_order_picks_lowest() {
        let candidates = vec![
            candidate("deepseek-harness", RuntimeStatus::Ready),
            candidate("codex", RuntimeStatus::Ready),
            candidate("claude-code", RuntimeStatus::Ready),
        ];
        let decision = route_runtime(None, false, &candidates).expect("有候选");
        assert_eq!(decision.runtime_id, "claude-code");
        assert_eq!(decision.priority, 10);
        assert!(decision.reason.contains("claude-code"));
        assert_eq!(decision.candidates.len(), 3);
    }

    #[test]
    fn ready_preferred_over_cold() {
        let candidates = vec![
            candidate("claude-code", RuntimeStatus::Cold),
            candidate("codex", RuntimeStatus::Ready),
        ];
        let decision = route_runtime(None, false, &candidates).expect("有候选");
        assert_eq!(decision.runtime_id, "codex", "ready 优先于 cold（即使优先级更差）");

        // 全部 cold 亦接受（入选并触发预热）。
        let colds = vec![
            candidate("deepseek-harness", RuntimeStatus::Cold),
            candidate("claude-code", RuntimeStatus::Cold),
        ];
        let decision = route_runtime(None, false, &colds).expect("cold 可参与");
        assert_eq!(decision.runtime_id, "claude-code");
    }

    #[test]
    fn equal_priority_falls_back_to_id_order() {
        let candidates = vec![
            candidate("zeta-runtime", RuntimeStatus::Ready),
            candidate("alpha-runtime", RuntimeStatus::Ready),
        ];
        let decision = route_runtime(None, false, &candidates).expect("有候选");
        assert_eq!(decision.runtime_id, "alpha-runtime", "相等按 id 字典序");
    }

    #[test]
    fn starting_and_degraded_are_excluded() {
        let candidates = vec![
            candidate("claude-code", RuntimeStatus::Starting),
            candidate("codex", RuntimeStatus::Degraded),
            candidate("deepseek-harness", RuntimeStatus::Cold),
        ];
        let decision = route_runtime(None, false, &candidates).expect("cold 可参与");
        assert_eq!(decision.runtime_id, "deepseek-harness");
        let traces = &decision.candidates;
        assert!(!traces[0].eligible);
        assert!(traces[0].detail.contains("starting"));
        assert!(!traces[1].eligible);
        assert!(traces[1].detail.contains("degraded"));
    }

    #[test]
    fn all_disabled_reports_failure_reason() {
        let candidates = vec![
            disabled("claude-code", RuntimeStatus::Disabled),
            disabled("codex", RuntimeStatus::Disabled),
        ];
        let failure = route_runtime(None, false, &candidates).expect_err("无候选");
        assert_eq!(failure, RouteFailure::AllDisabled);
        assert!(failure.reason().contains("全部已禁用"));
        // 空注册表同样归入「全部禁用」失败（不静默回退）。
        assert_eq!(
            route_runtime(None, false, &[]).expect_err("空"),
            RouteFailure::AllDisabled
        );
    }

    #[test]
    fn disabled_status_excluded_even_when_enabled_preference_true() {
        // 偏好 enabled=1 但监督器状态 disabled（如未同步窗口）→ 排除。
        let candidates = vec![candidate("claude-code", RuntimeStatus::Disabled)];
        assert_eq!(
            route_runtime(None, false, &candidates).expect_err("disabled"),
            RouteFailure::AllDisabled
        );
    }

    #[test]
    fn model_requires_enabled_provider_or_passthrough() {
        let candidates = vec![candidate("claude-code", RuntimeStatus::Ready)];
        // 无启用供应商提供该模型且候选不直通 → 无匹配模型。
        let failure =
            route_runtime(Some("deepseek-chat"), false, &candidates).expect_err("无匹配模型");
        assert_eq!(
            failure,
            RouteFailure::ModelUnavailable {
                model: "deepseek-chat".to_owned()
            }
        );
        assert!(failure.reason().contains("deepseek-chat"));

        // 存在启用 provider 提供该模型 → 参与路由。
        let decision =
            route_runtime(Some("deepseek-chat"), true, &candidates).expect("provider 提供模型");
        assert_eq!(decision.runtime_id, "claude-code");

        // 候选声明直通（DSH 按会话选模型）→ 参与路由。
        let passthrough = vec![RouteCandidate {
            id: "deepseek-harness".to_owned(),
            enabled: true,
            status: RuntimeStatus::Ready,
            model_passthrough: true,
        }];
        let decision =
            route_runtime(Some("any-model"), false, &passthrough).expect("直通候选");
        assert_eq!(decision.runtime_id, "deepseek-harness");
    }

    #[test]
    fn no_ready_candidate_after_model_filter() {
        let candidates = vec![candidate("claude-code", RuntimeStatus::Starting)];
        let failure = route_runtime(Some("m"), true, &candidates).expect_err("starting 排除");
        assert_eq!(failure, RouteFailure::NoReadyCandidate);
        assert!(failure.reason().contains("无 ready/cold 候选"));
    }

    #[test]
    fn trace_marks_excluded_reasons() {
        let candidates = vec![
            disabled("mock", RuntimeStatus::Disabled),
            candidate("codex", RuntimeStatus::Ready),
        ];
        let decision = route_runtime(None, false, &candidates).expect("codex 入选");
        assert_eq!(decision.runtime_id, "codex");
        let excluded = decision
            .candidates
            .iter()
            .find(|trace| trace.id == "mock")
            .expect("mock 追踪");
        assert!(!excluded.eligible);
        assert!(excluded.detail.starts_with("排除："));
    }
}
