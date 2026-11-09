//! M10-02/ADR-032：子 Agent 交付模式、上下文块与结果 schema（纯模型）。
//!
//! 本模块只定义跨层共享的稳定契约（core / control / app / adapters）：
//! - `subagent_context` 的交付模式（`message_append` / `origin_fork` / `system_replace`）；
//! - 父会话上下文块（版本化、可 ack，≤16KiB）；
//! - 结构化结果（canonical JSON，≤64KiB）与 compact envelope 上限。

use serde::{Deserialize, Serialize};

/// 单子 Agent 完整结果上限（ADR-032 D-032-4：64KiB）。
pub const SUBAGENT_RESULT_MAX_BYTES: usize = 64 * 1024;
/// compact envelope 单批上限（ADR-032 v1.1：8KiB/批）。
pub const SUBAGENT_ENVELOPE_MAX_BYTES: usize = 8 * 1024;
/// 父上下文注入块上限（D6 `session.send.subagent_context.block`：16KiB）。
pub const SUBAGENT_CONTEXT_MAX_BYTES: usize = 16 * 1024;
/// 组装层保存的最近 envelope 原文条数（ADR-032 v1.1：8 条）。
pub const SUBAGENT_FOLD_KEEP: usize = 8;
/// 折叠摘要上限（2KiB）。
pub const SUBAGENT_FOLD_SUMMARY_MAX_BYTES: usize = 2 * 1024;
/// `subagent_results.summary` / `SubagentNode.result_summary` 的 UTF-8 字节上限。
pub const SUBAGENT_SUMMARY_MAX_BYTES: usize = 512;
/// 单结果 `findings` 上限。
pub const SUBAGENT_FINDINGS_MAX: usize = 50;
/// 单 finding `evidence_refs` 上限。
pub const SUBAGENT_EVIDENCE_MAX: usize = 16;
/// 单父会话累计派生次数上限。
pub const SUBAGENT_SPAWN_COUNT_MAX: u64 = 64;
/// 单父会话活动子 Agent 上限。
pub const SUBAGENT_FANOUT_MAX: u64 = 4;
/// 全局并发子 Agent 上限。
pub const SUBAGENT_CONCURRENCY_MAX: u64 = 8;
/// 派生深度上限（根 0，子 1，孙 2；孙不可再派生）。
pub const SUBAGENT_DEPTH_MAX: u32 = 2;
/// `context_refs` 条数上限。
pub const SUBAGENT_CONTEXT_REFS_MAX: usize = 32;
/// `context_refs` 合计文本上限（32KiB）。
pub const SUBAGENT_CONTEXT_TEXT_MAX_BYTES: usize = 32 * 1024;
/// 单子 Agent 缺省超时（30min）。
pub const SUBAGENT_TIMEOUT_DEFAULT_MS: i64 = 30 * 60 * 1000;
/// 单子 Agent 超时上限（2h）。
pub const SUBAGENT_TIMEOUT_MAX_MS: i64 = 2 * 60 * 60 * 1000;

/// `subagent_context` 交付模式（ADR-032 v1.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentDeliveryMode {
    /// 默认：父 system prompt 不变，compact envelope 作为合成 user message 追加。
    MessageAppend,
    /// 后台：delegation boundary 冻结 origin fork + 独立 ephemeral context。
    OriginFork,
    /// 可选严格模式：替换 system 块（仅运行时声明支持）。
    SystemReplace,
}

impl SubagentDeliveryMode {
    pub const ALL: [Self; 3] = [Self::MessageAppend, Self::OriginFork, Self::SystemReplace];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MessageAppend => "message_append",
            Self::OriginFork => "origin_fork",
            Self::SystemReplace => "system_replace",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == code)
    }
}

/// `session.send.subagent_context` 载荷（D6；≤16KiB）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentContext {
    /// 父会话单调版本（每父会话递增）。
    pub version: u64,
    /// compact envelope 块（含 `<aether:subagent-results version=...>` 包装）。
    pub block: String,
    /// 交付模式（缺省 `message_append`；适配器必须显式支持）。
    #[serde(default = "default_delivery_mode")]
    pub mode: SubagentDeliveryMode,
}

fn default_delivery_mode() -> SubagentDeliveryMode {
    SubagentDeliveryMode::MessageAppend
}

/// `session.send` ack 的交付模式回显类型（D6）。
pub type AppliedSubagentMode = SubagentDeliveryMode;

/// 结构化结果 verdict（ADR-032 D-032-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentVerdict {
    Approve,
    Reject,
    NeedsChanges,
    Info,
}

impl SubagentVerdict {
    /// N→1 汇总优先级（`reject > needs_changes > approve > info`）。
    pub const fn priority(self) -> u8 {
        match self {
            Self::Reject => 4,
            Self::NeedsChanges => 3,
            Self::Approve => 2,
            Self::Info => 1,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::NeedsChanges => "needs_changes",
            Self::Info => "info",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        [Self::Approve, Self::Reject, Self::NeedsChanges, Self::Info]
            .into_iter()
            .find(|verdict| verdict.as_str() == code)
    }
}

/// finding 严重度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentSeverity {
    Blocker,
    Major,
    Minor,
    Info,
}

/// 证据引用（路径只作为文本证据；读取仍走 `fs.read` 权限门）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentEvidenceRef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}

/// 单条 finding。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentFinding {
    pub severity: SubagentSeverity,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub evidence_refs: Vec<SubagentEvidenceRef>,
}

/// 用量（结果 schema 内可选）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentUsage {
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
}

/// 子 Agent 结构化结果（canonical JSON；ADR-032 D-032-4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentResult {
    pub verdict: SubagentVerdict,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<SubagentFinding>,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub usage: SubagentUsage,
}

impl SubagentResult {
    /// 由不可解析的自由文本合成保守结果（`verdict=info`）。
    pub fn from_text(text: &str) -> Self {
        let mut summary = text.trim().to_owned();
        if summary.len() > 8192 {
            let mut end = 8192;
            while end > 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            summary.truncate(end);
        }
        Self {
            verdict: SubagentVerdict::Info,
            summary,
            findings: Vec::new(),
            confidence: 0.0,
            usage: SubagentUsage::default(),
        }
    }
}

/// UTF-8 边界截断（返回截断后的字符串与是否发生截断）。
pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}
