//! 附件重放解析接口（M10-05/ADR-035；主流 Agent 的 retry/resume 应重放完整输入）。
//!
//! 控制层不直接依赖 `data_dir`/存储路径解析/供应商模型能力；由 `aether-app` 注入实现：
//! - 从 `messages.content_parts` 读取附件引用；
//! - 重新校验会话归属、文件存在、内容哈希/大小、像素/限额；
//! - 重新执行运行时 `multimodal` 与模型 `vision` 能力门；
//! - 返回可直接透传 `session.send.attachments` 的 [`OutboundAttachment`]。
//!
//! 任一校验失败必须返回结构化错误（不静默降级为纯文本）。

use std::future::Future;
use std::pin::Pin;

use aether_core::{Message, OutboundAttachment, SessionId};

/// 重放附件失败分类（映射到 ADR-035 业务码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentResolveErrorKind {
    /// 运行时不支持 `multimodal` 或模型明确 `vision=false`。
    Unsupported,
    /// 文件/像素/字节超限。
    TooLarge,
    /// 单条消息附件数超限。
    LimitExceeded,
    /// 附件不存在/文件缺失/哈希不一致/路径校验失败。
    StorageFailed,
    /// `content_parts` 形状非法。
    Invalid,
}

/// 重放附件结构化错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentResolveError {
    pub kind: AttachmentResolveErrorKind,
    pub message: String,
}

impl AttachmentResolveError {
    pub fn new(kind: AttachmentResolveErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// 异步解析结果（object-safe）。
pub type AttachmentResolveFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<OutboundAttachment>, AttachmentResolveError>> + Send + 'a>,
>;

/// 附件重放解析器（由应用层注入；测试可注入替身）。
pub trait AttachmentResolver: Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        session_id: &'a SessionId,
        message: &'a Message,
    ) -> AttachmentResolveFuture<'a>;
}

/// 消息是否携带附件引用（`content_parts[].attachment_id`）。
#[must_use]
pub fn message_has_attachments(message: &Message) -> bool {
    message
        .content_parts
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| parts.iter().any(|part| part.get("attachment_id").is_some()))
}
