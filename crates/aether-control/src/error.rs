//! 事件管线错误类型（M1-05；设计 D4）。
//!
//! 错误码口径（供命令层 / 诊断 / 验收脚本断言）：
//! - `persist_degraded`：持久化降级（写事务连续失败 → 只读）；拒绝新写入/新 run；
//! - `storage_backpressure`：写队列临时高水位（D8 L2，**不**进入降级状态）；
//! - `readback_gap_too_large`：补读缺口 >10k，拒绝自动补发（D4 失败场景表）；
//! - `duplicate_event_id`：`evt.id` 幂等命中（丢弃，不视为故障）；
//! - `duplicate_seq`：`UNIQUE(session_id, seq)` 兜底命中（管线 bug，按持久化失败路径处理）。

use std::fmt;

/// 归一化阶段错误（D4：适配器事件 → 事件类型映射 → serde 严格校验）。
///
/// 校验失败按 D4 降级为 `log(warn)` + 死信计数，**不阻断会话**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// 信封/字段形状非法（缺字段、未知顶层字段、非法 ID 等；serde 严格模式）。
    Malformed { reason: String },
    /// 事件模型版本不受支持（跨 `v` 只允许新增字段，不允许未知版本）。
    UnsupportedVersion { found: u32, supported: u32 },
    /// `type` 不在附录 B 清单内（禁止自造事件类型）。
    UnknownEventType { event_type: String },
    /// 附录 B 预留类型在 P0 未启用（payload 语义未定义）。
    ReservedEventType { event_type: String },
    /// payload 与 `type` 不匹配或含未知字段（按 type 的扩展策略）。
    InvalidPayload { event_type: String, reason: String },
}

impl NormalizeError {
    /// 稳定错误码（死信计数按此分类）。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "malformed_event",
            Self::UnsupportedVersion { .. } => "unsupported_event_version",
            Self::UnknownEventType { .. } => "unknown_event_type",
            Self::ReservedEventType { .. } => "reserved_event_type",
            Self::InvalidPayload { .. } => "invalid_event_payload",
        }
    }
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { reason } => write!(f, "事件形状非法（严格校验拒绝）: {reason}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "事件模型版本不受支持: found={found}, supported={supported}（D4 信封 v）"
            ),
            Self::UnknownEventType { event_type } => {
                write!(f, "未知事件类型（附录 B 清单外）: {event_type}")
            }
            Self::ReservedEventType { event_type } => write!(
                f,
                "附录 B 预留事件类型在 P0 未启用: {event_type}（payload 语义未定义）"
            ),
            Self::InvalidPayload { event_type, reason } => {
                write!(f, "payload 校验失败（type={event_type}）: {reason}")
            }
        }
    }
}

impl std::error::Error for NormalizeError {}

/// journal（写队列）失败分类（D4：写失败重试 3 次 → `persist_degraded`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalError {
    /// 写事务失败（SQLite 扩展码 + 文案）。
    TransactionFailed { code: Option<i32>, message: String },
    /// `events.id` 主键冲突：`evt.id` 幂等命中（丢弃 + 计数，非持久化故障）。
    DuplicateEventId,
    /// `UNIQUE(session_id, seq)` 兜底命中：管线 bug，计入诊断并按持久化失败路径处理。
    DuplicateSeq { message: String },
    /// 写队列临时高水位（D8 L2；**不得**据此进入 `persist_degraded`）。
    Backpressure { depth: usize, threshold: usize },
    /// 写队列已关闭（写任务退出）。
    Closed,
    /// 其它 journal 失败（IO / 序列化等）。
    Other { message: String },
}

impl JournalError {
    /// 稳定错误码。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TransactionFailed { .. } => "journal_transaction_failed",
            Self::DuplicateEventId => "duplicate_event_id",
            Self::DuplicateSeq { .. } => "duplicate_seq",
            Self::Backpressure { .. } => "storage_backpressure",
            Self::Closed => "write_queue_closed",
            Self::Other { .. } => "journal_error",
        }
    }

    /// 诊断用单行描述。
    pub fn describe(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionFailed { code, message } => {
                let code = match code {
                    Some(value) => format!("SQLite 扩展码 {value}"),
                    None => "非 SQLite 错误".to_owned(),
                };
                write!(f, "写事务失败（{code}）：{message}")
            }
            Self::DuplicateEventId => {
                write!(f, "events.id 主键冲突（evt.id 幂等命中）→ 丢弃并计数")
            }
            Self::DuplicateSeq { message } => write!(
                f,
                "events UNIQUE(session_id, seq) 兜底命中（管线 bug）: {message}"
            ),
            Self::Backpressure { depth, threshold } => write!(
                f,
                "写队列临时高水位（storage_backpressure）：{depth} > {threshold}（D8 L2）"
            ),
            Self::Closed => write!(f, "写队列已关闭（写任务已退出）"),
            Self::Other { message } => write!(f, "journal 错误: {message}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// 读侧（补读 / sequencer 恢复）错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// 读连接不可用或查询失败。
    Unavailable { message: String },
}

impl SourceError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "source_unavailable",
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { message } => write!(f, "事件读取不可用: {message}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// 事件管线错误（命令层经 [`PipelineError::code`] 返回结构化错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// 持久化降级：拒绝新写入 / 新 run（D4 降级期语义 1）。
    PersistDegraded { reason: String },
    /// 写队列临时高水位：拒绝该次准入（D8 L2；不改变存储状态）。
    StorageBackpressure { depth: usize, threshold: usize },
    /// 补读缺口 >10k：拒绝自动补发，提示重新打开会话（D4 失败场景表）。
    ReadbackGapTooLarge { gap: u64, limit: u64 },
    /// journal（写队列）失败（未触发降级时回传）。
    Journal(JournalError),
    /// 读侧失败（补读 / sequencer 初始化）。
    Source(SourceError),
    /// 管线配置非法。
    InvalidConfig { reason: String },
    /// 管线已关闭（命令通道关闭）。
    PipelineClosed,
    /// 内部不变量被破坏。
    Internal { reason: String },
}

impl PipelineError {
    /// 稳定错误码（D4/DoD：`persist_degraded`、`readback_gap_too_large` 等）。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PersistDegraded { .. } => "persist_degraded",
            Self::StorageBackpressure { .. } => "storage_backpressure",
            Self::ReadbackGapTooLarge { .. } => "readback_gap_too_large",
            Self::Journal(error) => error.code(),
            Self::Source(error) => error.code(),
            Self::InvalidConfig { .. } => "invalid_pipeline_config",
            Self::PipelineClosed => "pipeline_closed",
            Self::Internal { .. } => "internal",
        }
    }

    /// 诊断用单行描述。
    pub fn describe(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersistDegraded { reason } => write!(
                f,
                "存储降级（persist_degraded）：拒绝新写入/新 run；{reason}；\
                 修复外部条件后重启核心并以启动自检恢复（P0 无热恢复）"
            ),
            Self::StorageBackpressure { depth, threshold } => write!(
                f,
                "存储写队列背压（storage_backpressure）：{depth} > {threshold}（D8 L2，临时高水位）"
            ),
            Self::ReadbackGapTooLarge { gap, limit } => write!(
                f,
                "补读缺口过大（readback_gap_too_large）：{gap} > {limit}，拒绝自动补发；\
                 请重新打开会话"
            ),
            Self::Journal(error) => error.fmt(f),
            Self::Source(error) => error.fmt(f),
            Self::InvalidConfig { reason } => write!(f, "管线配置非法: {reason}"),
            Self::PipelineClosed => write!(f, "事件管线已关闭"),
            Self::Internal { reason } => write!(f, "事件管线内部错误: {reason}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<JournalError> for PipelineError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<SourceError> for PipelineError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize_cases() -> Vec<(NormalizeError, &'static str)> {
        vec![
            (
                NormalizeError::Malformed {
                    reason: "missing field".to_owned(),
                },
                "malformed_event",
            ),
            (
                NormalizeError::UnsupportedVersion {
                    found: 2,
                    supported: 1,
                },
                "unsupported_event_version",
            ),
            (
                NormalizeError::UnknownEventType {
                    event_type: "bogus".to_owned(),
                },
                "unknown_event_type",
            ),
            (
                NormalizeError::ReservedEventType {
                    event_type: "workflow.*".to_owned(),
                },
                "reserved_event_type",
            ),
            (
                NormalizeError::InvalidPayload {
                    event_type: "log".to_owned(),
                    reason: "level".to_owned(),
                },
                "invalid_event_payload",
            ),
        ]
    }

    fn journal_cases() -> Vec<(JournalError, &'static str)> {
        vec![
            (
                JournalError::TransactionFailed {
                    code: Some(13),
                    message: "disk I/O error".to_owned(),
                },
                "journal_transaction_failed",
            ),
            (JournalError::DuplicateEventId, "duplicate_event_id"),
            (
                JournalError::DuplicateSeq {
                    message: "UNIQUE".to_owned(),
                },
                "duplicate_seq",
            ),
            (
                JournalError::Backpressure {
                    depth: 4_097,
                    threshold: 4_096,
                },
                "storage_backpressure",
            ),
            (JournalError::Closed, "write_queue_closed"),
            (
                JournalError::Other {
                    message: "boom".to_owned(),
                },
                "journal_error",
            ),
        ]
    }

    #[test]
    fn codes_are_stable_and_unique_within_families() {
        let mut normalize_codes: Vec<&str> = normalize_cases()
            .iter()
            .map(|(error, _)| error.code())
            .collect();
        let mut journal_codes: Vec<&str> = journal_cases()
            .iter()
            .map(|(error, _)| error.code())
            .collect();
        let mut pipeline_codes: Vec<&str> = vec![
            PipelineError::PersistDegraded {
                reason: String::new(),
            }
            .code(),
            PipelineError::StorageBackpressure {
                depth: 1,
                threshold: 0,
            }
            .code(),
            PipelineError::ReadbackGapTooLarge { gap: 1, limit: 1 }.code(),
            PipelineError::InvalidConfig {
                reason: String::new(),
            }
            .code(),
            PipelineError::PipelineClosed.code(),
            PipelineError::Internal {
                reason: String::new(),
            }
            .code(),
        ];
        for codes in [
            &mut normalize_codes,
            &mut journal_codes,
            &mut pipeline_codes,
        ] {
            let count = codes.len();
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(codes.len(), count, "族内错误码必须唯一: {codes:?}");
        }
        // 跨族复用是契约一致的显式行为（D8 `storage_backpressure`）。
        assert_eq!(
            JournalError::Backpressure {
                depth: 1,
                threshold: 0
            }
            .code(),
            PipelineError::StorageBackpressure {
                depth: 1,
                threshold: 0
            }
            .code()
        );
        for code in normalize_codes
            .iter()
            .chain(journal_codes.iter())
            .chain(pipeline_codes.iter())
        {
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "错误码必须为 snake_case: {code}"
            );
        }
    }

    #[test]
    fn expected_codes_are_pinned() {
        assert_eq!(
            PipelineError::PersistDegraded {
                reason: "x".to_owned()
            }
            .code(),
            "persist_degraded"
        );
        assert_eq!(
            PipelineError::ReadbackGapTooLarge {
                gap: 10_001,
                limit: 10_000
            }
            .code(),
            "readback_gap_too_large"
        );
        for (error, expected) in normalize_cases() {
            assert_eq!(error.code(), expected);
            assert!(!error.to_string().is_empty());
        }
        for (error, expected) in journal_cases() {
            assert_eq!(error.code(), expected);
            assert!(!error.describe().is_empty());
        }
        assert_eq!(
            PipelineError::Source(SourceError::Unavailable {
                message: "x".to_owned()
            })
            .code(),
            "source_unavailable"
        );
    }

    #[test]
    fn error_conversions_keep_code() {
        let journal = JournalError::Backpressure {
            depth: 10,
            threshold: 9,
        };
        let error = PipelineError::from(journal);
        assert_eq!(error.code(), "storage_backpressure");
        let error = PipelineError::from(SourceError::Unavailable {
            message: "closed".to_owned(),
        });
        assert_eq!(error.code(), "source_unavailable");
    }
}
