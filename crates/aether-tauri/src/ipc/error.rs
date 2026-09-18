//! IPC 结构化错误（设计 D7「命令参数校验」：校验失败返回结构化错误码，
//! 不落库、不透传下游）。
//!
//! 错误码为稳定契约，前端与后续里程碑按 `code` 分支处理；`message` 只用于展示，
//! 不参与逻辑判断。

use std::fmt;

use serde::Serialize;

/// 结构化错误码（稳定枚举，新增取值需走设计变更评审）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    /// 请求体不是 JSON 对象，或 JSON 解析失败。
    InvalidJson,
    /// 出现未声明字段（serde `deny_unknown_fields`）。
    UnknownField,
    /// 缺少必填字段。
    MissingField,
    /// 字段类型错误。
    InvalidType,
    /// 字段取值非法（含数值越界）。
    InvalidValue,
    /// 枚举值不在白名单内。
    InvalidEnum,
    /// 超出长度上限（消息 1MiB / 标题 256 字符 / 分页 500 条等）。
    TooLarge,
    /// 数值超出允许区间。
    OutOfRange,
    /// 格式非法（ULID、标识符、控制字符等）。
    InvalidFormat,
    /// 路径校验失败（canonicalize / 白名单 / Windows 特殊路径）。
    PathRejected,
    /// 启动门阻断（M1-06/A4：数据目录检测未通过，或启动自检未完成）。
    StartupBlocked,
    /// 数据目录迁移失败（复制/校验/原子替换/指针锁定，M1-06）。
    MigrationFailed,
    /// 内部错误（序列化/任务调度失败等不可达路径；M1-06 启动门与迁移接线）。
    Internal,
    /// 核心后端尚未就绪（ADR-007 增量 2）：Builder 阶段以延迟后端管理状态，
    /// `setup`（单实例插件之后）完成存储/管线注入前的过渡窗口可达。
    CoreNotReady,
    /// 命令尚未实现（框架就绪，实现随对应里程碑落地）。
    NotImplemented,
}

impl IpcErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::UnknownField => "unknown_field",
            Self::MissingField => "missing_field",
            Self::InvalidType => "invalid_type",
            Self::InvalidValue => "invalid_value",
            Self::InvalidEnum => "invalid_enum",
            Self::TooLarge => "too_large",
            Self::OutOfRange => "out_of_range",
            Self::InvalidFormat => "invalid_format",
            Self::PathRejected => "path_rejected",
            Self::StartupBlocked => "startup_blocked",
            Self::MigrationFailed => "migration_failed",
            Self::Internal => "internal",
            Self::CoreNotReady => "core_not_ready",
            Self::NotImplemented => "not_implemented",
        }
    }
}

/// IPC 命令错误的线上形态：`{ "code": "...", "message": "...", "field": "..." }`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl IpcError {
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
        }
    }

    pub fn at_field(
        code: IpcErrorCode,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            field: Some(field.into()),
        }
    }

    pub fn invalid_json(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::InvalidJson, message)
    }

    pub fn unknown_field(field: impl Into<String>) -> Self {
        Self::at_field(
            IpcErrorCode::UnknownField,
            field,
            "出现未声明字段；严格模式下未知字段一律拒绝",
        )
    }

    pub fn missing_field(field: impl Into<String>) -> Self {
        Self::at_field(IpcErrorCode::MissingField, field, "缺少必填字段")
    }

    pub fn invalid_type(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::InvalidType, message)
    }

    pub fn invalid_value(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::InvalidValue, message)
    }

    pub fn invalid_enum(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at_field(IpcErrorCode::InvalidEnum, field, message)
    }

    pub fn too_large(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at_field(IpcErrorCode::TooLarge, field, message)
    }

    pub fn out_of_range(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at_field(IpcErrorCode::OutOfRange, field, message)
    }

    pub fn invalid_format(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at_field(IpcErrorCode::InvalidFormat, field, message)
    }

    pub fn path_rejected(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::PathRejected, message)
    }

    /// M1-06/A4：启动门阻断（仅「迁移/退出」可达）。
    pub fn startup_blocked(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::StartupBlocked, message)
    }

    /// M1-06：数据目录迁移失败。
    pub fn migration_failed(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::MigrationFailed, message)
    }

    /// M1-06：内部不可达错误（序列化失败等）。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::Internal, message)
    }

    /// ADR-007 增量 2：核心后端未就绪（启动序列尚未完成存储/管线注入）。
    ///
    /// 触发窗口：Builder 阶段以延迟后端 `manage` 状态、`setup` 注入真实后端之前。
    /// 门命令（`startup_*`）不依赖后端，该窗口内仍可用。
    pub fn core_not_ready(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::CoreNotReady, message)
    }

    pub fn not_implemented(command: &str) -> Self {
        Self::new(
            IpcErrorCode::NotImplemented,
            format!("命令 {command} 尚未实现；参数校验框架已生效（M1-08）"),
        )
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(
                f,
                "{}（字段 {field}）：{}",
                self.code.as_str(),
                self.message
            ),
            None => write!(f, "{}：{}", self.code.as_str(), self.message),
        }
    }
}

impl std::error::Error for IpcError {}
