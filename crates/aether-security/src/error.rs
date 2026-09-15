//! 密钥子系统错误类型（设计 D10 / A3）。
//!
//! 约束（AGENTS.md §2.9）：错误信息只允许包含引用 URI、平台错误文本与静态说明，
//! 禁止携带任何密钥值。

use crate::KeychainRef;

/// 密钥子系统统一错误。
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// `keychain://` 引用语法或字段非法。
    #[error("密钥引用非法：{0}")]
    InvalidReference(String),
    /// 注入项的环境变量名非法。
    #[error("环境变量名非法：{0}")]
    InvalidEnvName(String),
    /// OS 凭据库不可用（缺失、被拒、平台错误）。
    #[error("OS 凭据库不可用：{0}")]
    KeychainUnavailable(String),
    /// 引用目标不存在。
    #[error("密钥不存在：{reference}")]
    NotFound { reference: KeychainRef },
    /// 启动自检失败（写入→读取→删除链路上任一步不符合预期）。
    #[error("密钥存储自检失败：{0}")]
    SelfCheck(&'static str),
    /// 降级路径缺少口令（UI 必须要求输入，不得静默继续）。
    #[error("降级加密文件需要口令，当前未提供")]
    PassphraseRequired,
    /// 口令错误或密钥文件被篡改/损坏（不区分二者，避免侧信道）。
    #[error("口令错误或密钥文件损坏")]
    Decrypt,
    /// 密钥文件格式非法。
    #[error("密钥文件格式非法：{0}")]
    InvalidFormat(String),
    /// 随机数生成失败。
    #[error("随机数生成失败")]
    Random,
    /// 脱敏规则编译失败（编程错误，正常构建不可达）。
    #[error("脱敏规则初始化失败")]
    RedactorInit,
    /// 文件系统错误。
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化错误。
    #[error("序列化错误：{0}")]
    Serialization(#[from] serde_json::Error),
}

impl SecretError {
    /// 是否为「引用不存在」类错误。
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}
