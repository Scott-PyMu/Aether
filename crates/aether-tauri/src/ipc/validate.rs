//! IPC 参数校验框架（设计 D7 安全基线 + 评审 #7）。
//!
//! 流程固定为：`serde 严格反序列化（deny_unknown_fields）` → `语义校验
//! （长度上限 / 枚举白名单 / 格式）` → 才允许进入命令实现。任一步失败都返回
//! [`IpcError`]，不调用下游后端（即不落库、不透传）。
//!
//! 长度上限（设计 D7）：
//! - 消息文本 ≤ 1MiB（按 UTF-8 字节数）；
//! - 分页 ≤ 500 条；
//! - 会话标题 ≤ 256 字符（Unicode 标量值计数）。

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::IpcError;

/// 消息文本上限：1MiB（设计 D7）。
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// 会话标题上限：256 字符（设计 D7）。
pub const MAX_TITLE_CHARS: usize = 256;
/// 分页上限：500 条（设计 D7）。
pub const MAX_PAGE_LIMIT: u32 = 500;
/// 会话级模型覆盖上限（D6 `session.create` 参数）。
pub const MAX_MODEL_CHARS: usize = 128;
/// 备份标签上限（M3-04 使用）。
pub const MAX_LABEL_CHARS: usize = 128;
/// 路径字符串上限（Windows 长路径 32767，这里收紧到 4096）。
pub const MAX_PATH_CHARS: usize = 4096;
/// 设置值 JSON 序列化后的上限。
pub const MAX_SETTING_VALUE_BYTES: usize = 64 * 1024;
/// 标识符类字段（runtime_id / client_msg_id）上限。
pub const MAX_IDENTIFIER_CHARS: usize = 128;

/// 可作为 IPC 命令参数的请求体。
///
/// 实现者必须使用 `#[serde(deny_unknown_fields)]`，并在 [`CommandRequest::validate`]
/// 内完成长度 / 枚举 / 格式校验（路径校验见 [`super::path`]）。
pub trait CommandRequest: DeserializeOwned {
    fn validate(&self) -> Result<(), IpcError> {
        Ok(())
    }
}

/// 严格解析 + 语义校验；失败即结构化错误，调用方不得继续调用下游。
pub fn parse_strict<T: CommandRequest>(value: Value) -> Result<T, IpcError> {
    if !value.is_object() {
        return Err(IpcError::invalid_json(
            "命令参数必须是 JSON 对象（严格模式拒绝 null/数组/标量）",
        ));
    }
    let request: T = serde_json::from_value(value).map_err(map_serde_error)?;
    request.validate()?;
    Ok(request)
}

/// 无参数命令的严格解析（ADR-004：`backup_list` 无参数）。
///
/// 缺省调用（`null` 载荷）等价于空对象；任何成员都会被 `deny_unknown_fields` 拒绝。
pub fn parse_no_params<T: CommandRequest>(value: Value) -> Result<T, IpcError> {
    let value = if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    };
    parse_strict(value)
}

/// 把 serde 反序列化错误映射为稳定错误码。
///
/// serde_json 的错误文本是事实上的稳定接口（`unknown field \`x\`` /
/// `missing field \`x\`` / `invalid type: ...` / `invalid value: ...` /
/// `unknown variant \`x\``），由其前缀分类；分类结果由单测矩阵锁定。
pub fn map_serde_error(err: serde_json::Error) -> IpcError {
    let text = err.to_string();
    if let Some(field) = extract_quoted_after(&text, "unknown field ") {
        return IpcError::unknown_field(field);
    }
    if let Some(field) = extract_quoted_after(&text, "missing field ") {
        return IpcError::missing_field(field);
    }
    if text.starts_with("unknown variant") {
        return IpcError::invalid_enum("enum", text);
    }
    if text.starts_with("invalid type") {
        return IpcError::invalid_type(text);
    }
    if text.starts_with("invalid value") {
        return IpcError::invalid_value(text);
    }
    IpcError::invalid_json(text)
}

fn extract_quoted_after(text: &str, prefix: &str) -> Option<String> {
    let rest = text.strip_prefix(prefix)?;
    let start = rest.find('`')? + 1;
    let remainder = &rest[start..];
    let end = remainder.find('`')?;
    Some(remainder[..end].to_string())
}

/// ULID 词法校验（Crockford Base32，26 字符大写；首字符保证 128 位）。
pub fn is_ulid(value: &str) -> bool {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    value.len() == 26
        && value.bytes().all(|byte| ALPHABET.contains(&byte))
        && value.bytes().next().is_some_and(|first| first <= b'7')
}

/// 标识符校验：`[a-z0-9._-]`，首字符为字母或数字，长度 ≤ [`MAX_IDENTIFIER_CHARS`]。
pub fn validate_identifier(value: &str, field: &str) -> Result<(), IpcError> {
    if value.is_empty() || value.chars().count() > MAX_IDENTIFIER_CHARS {
        return Err(IpcError::out_of_range(
            field,
            format!("长度必须在 1..={MAX_IDENTIFIER_CHARS} 之间"),
        ));
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(IpcError::invalid_format(field, "不允许为空"));
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(IpcError::invalid_format(field, "必须以小写字母或数字开头"));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(IpcError::invalid_format(
            field,
            "仅允许小写字母、数字与 . _ -",
        ));
    }
    Ok(())
}

/// 通用标识符（含大写）校验：用于 `client_msg_id` 等外部生成 id。
pub fn validate_ascii_id(value: &str, field: &str) -> Result<(), IpcError> {
    if value.is_empty() || value.chars().count() > MAX_IDENTIFIER_CHARS {
        return Err(IpcError::out_of_range(
            field,
            format!("长度必须在 1..={MAX_IDENTIFIER_CHARS} 之间"),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(IpcError::invalid_format(
            field,
            "仅允许 ASCII 字母、数字与 . _ - :",
        ));
    }
    Ok(())
}

/// 模型名校验（D6 `session.create` 透传字段）。
pub fn validate_model(value: &str, field: &str) -> Result<(), IpcError> {
    if value.is_empty() || value.chars().count() > MAX_MODEL_CHARS {
        return Err(IpcError::out_of_range(
            field,
            format!("长度必须在 1..={MAX_MODEL_CHARS} 之间"),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+'))
    {
        return Err(IpcError::invalid_format(
            field,
            "仅允许 ASCII 字母、数字与 . _ - : / @ +",
        ));
    }
    Ok(())
}

/// 文本长度（UTF-8 字节）上限校验。
pub fn ensure_max_bytes(value: &str, field: &str, max_bytes: usize) -> Result<(), IpcError> {
    if value.len() > max_bytes {
        return Err(IpcError::too_large(
            field,
            format!("UTF-8 字节数 {} 超过上限 {max_bytes}", value.len()),
        ));
    }
    Ok(())
}

/// 文本长度（Unicode 标量值）上限校验。
pub fn ensure_max_chars(value: &str, field: &str, max_chars: usize) -> Result<(), IpcError> {
    let count = value.chars().count();
    if count > max_chars {
        return Err(IpcError::too_large(
            field,
            format!("字符数 {count} 超过上限 {max_chars}"),
        ));
    }
    Ok(())
}

/// 控制字符校验：标题/标签不允许任何控制字符；消息文本允许换行与制表符。
pub fn reject_control_chars(
    value: &str,
    field: &str,
    allow_whitespace_controls: bool,
) -> Result<(), IpcError> {
    let allowed = |c: char| allow_whitespace_controls && matches!(c, '\n' | '\r' | '\t');
    if value.chars().any(|c| c.is_control() && !allowed(c)) {
        return Err(IpcError::invalid_format(field, "包含不允许的控制字符"));
    }
    Ok(())
}

/// 非空校验。
pub fn ensure_not_empty(value: &str, field: &str) -> Result<(), IpcError> {
    if value.is_empty() {
        return Err(IpcError::invalid_format(field, "不允许为空"));
    }
    Ok(())
}

/// JSON 值的序列化字节数上限（设置项等自由形态字段）。
pub fn ensure_value_size(value: &Value, field: &str, max_bytes: usize) -> Result<(), IpcError> {
    let size = serde_json::to_vec(value).map_err(|_| IpcError::invalid_value("值不可序列化"))?;
    if size.len() > max_bytes {
        return Err(IpcError::too_large(
            field,
            format!("JSON 字节数 {} 超过上限 {max_bytes}", size.len()),
        ));
    }
    Ok(())
}
