//! 响应预算与不透明游标（ADR-009 §9 / ADR-018 附录 A-1；M9-04 DoD-4）。
//!
//! 口径：
//! - `max_bytes` 缺省 **1MiB**、下限 **64KiB**、上限 **1.5MiB**；越界 `out_of_range`；
//! - 每页**至少返回 1 条**（允许略超预算）；整帧仍 ≤2MiB（单条上限 1MiB）；
//! - `next_cursor` 为不透明 base64url（无填充）JSON `{v,stream,pos,filter_hash}`；
//!   客户端不得解析/构造；服务端每次重算并校验 `filter_hash`，不匹配 → `invalid_value`；
//! - `messages_page` 的 `next_cursor` 仅作用于 **events 补读流**；`audit_query` 的
//!   `next_cursor` 与 `offset` 互斥；
//! - 零推进（分页无新条目）不得返回同一游标，避免客户端死循环。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{IpcError, IpcErrorCode};

/// `max_bytes` 下限：64KiB。
pub const MAX_BYTES_MIN: u32 = 64 * 1024;
/// `max_bytes` 缺省值：1MiB。
pub const MAX_BYTES_DEFAULT: u32 = 1024 * 1024;
/// `max_bytes` 上限：1.5MiB。
pub const MAX_BYTES_MAX: u32 = 1536 * 1024;
/// 线协议单帧上限（D6；`aether-rpc::MAX_FRAME_BYTES` 同值）。
pub const FRAME_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// 单条载荷上限（D7：单条 ≤1MiB）。
pub const ITEM_LIMIT_BYTES: usize = 1024 * 1024;
/// 游标版本（结构变更须升版；旧版本一律 `invalid_value`）。
pub const CURSOR_VERSION: u32 = 1;
/// events 补读流标识。
pub const STREAM_EVENTS: &str = "events";
/// 审计查询流标识。
pub const STREAM_AUDIT: &str = "audit";

/// 游标载荷（线上为 base64url(JSON)；**不透明**，仅服务端构造/解析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorPayload {
    v: u32,
    stream: String,
    pos: u64,
    filter_hash: String,
}

/// 校验并归一化 `max_bytes`（`None` = 缺省 1MiB）。
pub fn validate_max_bytes(value: Option<u32>, field: &str) -> Result<u32, IpcError> {
    let value = value.unwrap_or(MAX_BYTES_DEFAULT);
    if !(MAX_BYTES_MIN..=MAX_BYTES_MAX).contains(&value) {
        return Err(IpcError::out_of_range(
            field,
            format!(
                "必须在 {MAX_BYTES_MIN}..={MAX_BYTES_MAX} 字节之间（64KiB–1.5MiB；缺省 {MAX_BYTES_DEFAULT}）"
            ),
        ));
    }
    Ok(value)
}

/// 过滤条件指纹：同一分页流 + 同一过滤条件才能复用游标。
pub fn filter_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0x1f]);
    }
    let digest = hasher.finalize();
    let mut text = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 构造不透明游标（`v/stream/pos/filter_hash`）。
///
/// 入参只来自内部常量与 SHA-256 hex，JSON 无需转义（不引入失败分支）；
/// 解析仍用 [`CursorPayload`] 的 `deny_unknown_fields` 严格校验。
pub fn encode_cursor(stream: &str, pos: u64, filter_hash: &str) -> String {
    let payload = format!(
        r#"{{"v":{CURSOR_VERSION},"stream":"{stream}","pos":{pos},"filter_hash":"{filter_hash}"}}"#
    );
    base64url_encode(payload.as_bytes())
}

/// 解析并校验游标结构 + 流标识；`filter_hash` 由调用方比对。
pub fn decode_cursor(token: &str, stream: &str) -> Result<(u64, String), IpcError> {
    let bytes = base64url_decode(token).ok_or_else(|| {
        IpcError::at_field(
            IpcErrorCode::InvalidValue,
            "next_cursor",
            "游标不是合法的 base64url（不透明游标不得手工构造）",
        )
    })?;
    let payload: CursorPayload = serde_json::from_slice(&bytes).map_err(|error| {
        IpcError::at_field(
            IpcErrorCode::InvalidValue,
            "next_cursor",
            format!("游标结构非法（{error}）；不得手工构造"),
        )
    })?;
    if payload.v != CURSOR_VERSION {
        return Err(IpcError::at_field(
            IpcErrorCode::InvalidValue,
            "next_cursor",
            format!("游标版本 {} 不受支持（当前 {CURSOR_VERSION}）", payload.v),
        ));
    }
    if payload.stream != stream {
        return Err(IpcError::at_field(
            IpcErrorCode::InvalidValue,
            "next_cursor",
            format!(
                "游标流 {} 与当前查询不匹配（期望 {stream}）",
                payload.stream
            ),
        ));
    }
    if payload.filter_hash.is_empty() {
        return Err(IpcError::at_field(
            IpcErrorCode::InvalidValue,
            "next_cursor",
            "游标缺少 filter_hash",
        ));
    }
    Ok((payload.pos, payload.filter_hash))
}

/// 仅做结构/流校验（DTO 校验阶段；`filter_hash` 比对在拥有过滤条件的后端层）。
pub fn validate_cursor(token: &str, stream: &str) -> Result<(), IpcError> {
    decode_cursor(token, stream).map(|_| ())
}

/// 按预算裁剪（从头保留）：至少 1 条；返回 `(保留条目, 是否因预算截断)`。
pub fn fit_budget<T: Serialize>(
    items: Vec<T>,
    max_bytes: usize,
) -> Result<(Vec<T>, bool), IpcError> {
    trim_budget(items, max_bytes, false)
}

/// 按预算裁剪（从尾部保留最近条目；用于「最近一页」尾语义）。
pub fn fit_budget_tail<T: Serialize>(
    items: Vec<T>,
    max_bytes: usize,
) -> Result<(Vec<T>, bool), IpcError> {
    trim_budget(items, max_bytes, true)
}

fn trim_budget<T: Serialize>(
    items: Vec<T>,
    max_bytes: usize,
    from_tail: bool,
) -> Result<(Vec<T>, bool), IpcError> {
    if items.is_empty() {
        return Ok((items, false));
    }
    let total = items.len();
    let mut sized: Vec<(usize, T)> = Vec::with_capacity(total);
    for item in items {
        let bytes = serde_json::to_vec(&item)
            .map_err(|error| IpcError::internal(format!("分页载荷序列化失败：{error}")))?
            .len();
        sized.push((bytes, item));
    }
    let first_bytes = if from_tail {
        sized.last().map(|(bytes, _)| *bytes).unwrap_or(0)
    } else {
        sized.first().map(|(bytes, _)| *bytes).unwrap_or(0)
    };
    if first_bytes > ITEM_LIMIT_BYTES {
        return Err(IpcError::too_large(
            "result",
            format!("单条载荷 {first_bytes} 字节超过上限 {ITEM_LIMIT_BYTES}（D7 单条 ≤1MiB）"),
        ));
    }
    let mut used = 0usize;
    let mut kept = 0usize;
    if from_tail {
        for (bytes, _) in sized.iter().rev() {
            let cost = bytes.saturating_add(usize::from(kept > 0));
            if used.saturating_add(cost) > max_bytes && kept > 0 {
                break;
            }
            used = used.saturating_add(cost);
            kept += 1;
        }
        let start = sized.len().saturating_sub(kept);
        let truncated = start > 0;
        let kept_items = sized
            .into_iter()
            .skip(start)
            .map(|(_, item)| item)
            .collect();
        Ok((kept_items, truncated))
    } else {
        for (bytes, _) in &sized {
            let cost = bytes.saturating_add(usize::from(kept > 0));
            if used.saturating_add(cost) > max_bytes && kept > 0 {
                break;
            }
            used = used.saturating_add(cost);
            kept += 1;
        }
        let truncated = kept < sized.len();
        let kept_items = sized.into_iter().take(kept).map(|(_, item)| item).collect();
        Ok((kept_items, truncated))
    }
}

/// 序列化后的载荷大小（预算断言/诊断用）。
pub fn value_bytes<T: Serialize>(value: &T) -> Result<usize, IpcError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| IpcError::internal(format!("载荷序列化失败：{error}")))
}

/// 过滤条件指纹里统一用稳定文本（`None` = 空串）。
pub fn stable_text(value: Option<&str>) -> &str {
    value.unwrap_or("")
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let b2 = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut triple = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            triple |= value(*byte)? << (18 - index * 6);
        }
        out.push(((triple >> 16) & 0xff) as u8);
        if chunk.len() > 2 {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if chunk.len() > 3 {
            out.push((triple & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn cursor_round_trip_is_opaque_base64url() {
        let hash = filter_hash(&[STREAM_EVENTS, "01J00000000000000000000000", "500"]);
        let token = encode_cursor(STREAM_EVENTS, 42, &hash);
        assert!(
            token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
            "游标必须是 base64url（无填充）：{token}"
        );
        let (pos, decoded_hash) = decode_cursor(&token, STREAM_EVENTS).expect("游标可解析");
        assert_eq!(pos, 42);
        assert_eq!(decoded_hash, hash);
    }

    #[test]
    fn tampered_or_mismatched_cursor_is_invalid_value() {
        let hash = filter_hash(&[STREAM_AUDIT, "50"]);
        let token = encode_cursor(STREAM_AUDIT, 10, &hash);
        // 流不匹配
        let error = decode_cursor(&token, STREAM_EVENTS).expect_err("流不匹配必须拒绝");
        assert_eq!(error.code, IpcErrorCode::InvalidValue);
        assert_eq!(error.field.as_deref(), Some("next_cursor"));
        // 手工构造/篡改（非法 base64url）
        let error = decode_cursor("not*base64url", STREAM_AUDIT).expect_err("非法游标必须拒绝");
        assert_eq!(error.code, IpcErrorCode::InvalidValue);
        // 合法 base64url 但 JSON 结构不符
        let error = decode_cursor(&base64url_encode(b"{\"v\":1}"), STREAM_AUDIT)
            .expect_err("结构非法必须拒绝");
        assert_eq!(error.code, IpcErrorCode::InvalidValue);
        // filter_hash 不匹配由调用方比对（见 audit/messages 用例）。
        let (_, other) = decode_cursor(&token, STREAM_AUDIT).expect("结构合法");
        assert_ne!(other, filter_hash(&[STREAM_AUDIT, "51"]));
    }

    #[test]
    fn max_bytes_bounds_are_frozen() {
        assert_eq!(
            validate_max_bytes(None, "max_bytes").unwrap(),
            MAX_BYTES_DEFAULT
        );
        assert_eq!(
            validate_max_bytes(Some(MAX_BYTES_MIN), "max_bytes").unwrap(),
            MAX_BYTES_MIN
        );
        assert_eq!(
            validate_max_bytes(Some(MAX_BYTES_MAX), "max_bytes").unwrap(),
            MAX_BYTES_MAX
        );
        for value in [MAX_BYTES_MIN - 1, MAX_BYTES_MAX + 1] {
            let error = validate_max_bytes(Some(value), "max_bytes").expect_err("越界必须拒绝");
            assert_eq!(error.code, IpcErrorCode::OutOfRange);
            assert_eq!(error.field.as_deref(), Some("max_bytes"));
        }
    }

    #[test]
    fn budget_keeps_at_least_one_item_and_reports_truncation() {
        let items = vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 3})];
        let (kept, truncated) = fit_budget(items, 1).expect("至少 1 条");
        assert_eq!(kept.len(), 1);
        assert!(truncated, "预算不足必须标记截断");

        let items = vec![json!({"n": 1}), json!({"n": 2})];
        let (kept, truncated) = fit_budget(items, MAX_BYTES_MIN as usize).expect("预算充足");
        assert_eq!(kept.len(), 2);
        assert!(!truncated);

        let items = vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 3})];
        let (kept, truncated) = fit_budget_tail(items, 12).expect("尾页至少 1 条");
        assert_eq!(kept.len(), 1, "尾页保留最近条目");
        assert_eq!(kept[0]["n"], json!(3));
        assert!(truncated);
    }

    #[test]
    fn single_item_over_one_mib_is_too_large() {
        let item = json!({ "message": "x".repeat(ITEM_LIMIT_BYTES + 1) });
        let error = fit_budget(vec![item], MAX_BYTES_MIN as usize).expect_err("单条超限必须拒绝");
        assert_eq!(error.code, IpcErrorCode::TooLarge);
    }

    #[test]
    fn zero_progress_has_no_cursor() {
        // 空页不得产生游标（调用方以 `events.is_empty()`/`rows.is_empty()` 判定）。
        let (kept, truncated) = fit_budget(Vec::<Value>::new(), 1).expect("空页合法");
        assert!(kept.is_empty());
        assert!(!truncated);
    }
}
